use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::model::{Provider, ProviderSnapshot, ServiceStatus, ServiceStatusLevel};
use crate::settings::{ProviderSettings, Settings};
use crate::{diagnostics, providers, service_status, visibility};

pub const WM_USAGE_UPDATED: u32 = 0x8001;
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug)]
pub enum RefreshCommand {
    RefreshAll,
    RefreshUsage,
    Quit,
}

pub struct SharedState {
    snapshots: Mutex<[ProviderSnapshot; Provider::COUNT]>,
    service_statuses: Mutex<[ServiceStatus; Provider::COUNT]>,
    refresh_tx: mpsc::Sender<RefreshCommand>,
    tray_hwnd: AtomicIsize,
    dashboard_hwnd: AtomicIsize,
    pub refreshing: AtomicBool,
    pub start_with_windows: AtomicBool,
    visible_providers: AtomicU8,
    pub icon_handles: Mutex<[Option<isize>; Provider::COUNT]>,
}

impl SharedState {
    pub fn new() -> (Arc<Self>, mpsc::Receiver<RefreshCommand>) {
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let state = Arc::new(Self {
            snapshots: Mutex::new([
                ProviderSnapshot::empty(Provider::Claude),
                ProviderSnapshot::empty(Provider::Codex),
            ]),
            service_statuses: Mutex::new(std::array::from_fn(|_| {
                ServiceStatus::new(ServiceStatusLevel::Unavailable)
            })),
            refresh_tx,
            tray_hwnd: AtomicIsize::new(0),
            dashboard_hwnd: AtomicIsize::new(0),
            refreshing: AtomicBool::new(false),
            start_with_windows: AtomicBool::new(false),
            visible_providers: AtomicU8::new(visibility::load()),
            icon_handles: Mutex::new([None; Provider::COUNT]),
        });
        (state, refresh_rx)
    }

    pub fn tray_window(&self) -> Option<HWND> {
        window(&self.tray_hwnd)
    }

    pub fn set_tray_window(&self, hwnd: HWND) {
        self.tray_hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
    }

    pub fn dashboard_window(&self) -> Option<HWND> {
        window(&self.dashboard_hwnd)
    }

    pub fn set_dashboard_window(&self, hwnd: HWND) {
        self.dashboard_hwnd.store(hwnd.0 as isize, Ordering::Relaxed);
    }

    pub fn snapshot(&self, provider: Provider) -> ProviderSnapshot {
        self.snapshots.lock().unwrap()[provider.index()].clone()
    }

    pub fn service_status(&self, provider: Provider) -> ServiceStatus {
        self.service_statuses.lock().unwrap()[provider.index()].clone()
    }

    pub fn request_refresh(&self) {
        let _ = self.refresh_tx.send(RefreshCommand::RefreshAll);
    }

    pub fn request_usage_refresh(&self) {
        let _ = self.refresh_tx.send(RefreshCommand::RefreshUsage);
    }

    pub fn request_quit(&self) {
        let _ = self.refresh_tx.send(RefreshCommand::Quit);
    }

    pub fn is_provider_visible(&self, provider: Provider) -> bool {
        self.visible_providers.load(Ordering::Relaxed) & (1 << provider.index()) != 0
    }

    pub fn toggle_provider(&self, provider: Provider) -> bool {
        let bit = 1 << provider.index();
        let current = self.visible_providers.load(Ordering::Relaxed);
        let next = current ^ bit;
        if next == 0 {
            return false;
        }
        self.visible_providers.store(next, Ordering::Relaxed);
        if let Err(error) = visibility::save(next) {
            diagnostics::event("WARN", &error);
        }
        true
    }

    fn refresh_usage(&self, provider: Provider, previous_config: &mut Option<ProviderSettings>) {
        diagnostics::event(
            "INFO",
            &format!("refreshing {} usage", provider.name()),
        );
        let result = self.read_usage(provider, previous_config);
        let mut snapshots = self.snapshots.lock().unwrap();
        let target = &mut snapshots[provider.index()];
        match result {
            Ok(snapshot) => {
                if target.error.is_some() {
                    diagnostics::event("INFO", &format!("{} usage recovered", provider.name()));
                }
                *target = snapshot;
            }
            Err(error) => {
                if target.error.as_deref() != Some(error.as_str()) {
                    diagnostics::event(
                        "WARN",
                        &format!("{} usage refresh failed: {error}", provider.name()),
                    );
                }
                target.record_error(error);
            }
        }
        drop(snapshots);
        self.notify_ui();
    }

    fn read_usage(
        &self,
        provider: Provider,
        previous_config: &mut Option<ProviderSettings>,
    ) -> Result<ProviderSnapshot, String> {
        let config = match Settings::load() {
            Ok(settings) => settings.providers[provider.index()].clone(),
            Err(error) => {
                self.clear_snapshot(provider);
                return Err(error);
            }
        };
        if previous_config.as_ref() != Some(&config) {
            self.clear_snapshot(provider);
        }
        *previous_config = Some(config.clone());
        let result = providers::refresh(provider, &config);
        let unchanged =
            Settings::load().is_ok_and(|settings| settings.providers[provider.index()] == config);
        if unchanged {
            return result;
        }
        self.clear_snapshot(provider);
        self.request_usage_refresh();
        Err("Source settings changed. Refreshing the selected source.".into())
    }

    fn clear_snapshot(&self, provider: Provider) {
        self.snapshots.lock().unwrap()[provider.index()] = ProviderSnapshot::empty(provider);
        self.notify_ui();
    }

    fn refresh_service_status(&self, provider: Provider) {
        let result = service_status::refresh(provider);
        let mut statuses = self.service_statuses.lock().unwrap();
        let target = &mut statuses[provider.index()];
        match result {
            Ok(status) => {
                if *target != status {
                    diagnostics::event(
                        "INFO",
                        &format!("{} service status: {}", provider.name(), status.label()),
                    );
                }
                *target = status;
            }
            Err(error) => {
                if target.error.as_deref() != Some(error.as_str()) {
                    diagnostics::event(
                        "WARN",
                        &format!("{} status refresh failed: {error}", provider.name()),
                    );
                }
                *target = ServiceStatus::from_error(error);
            }
        }
        drop(statuses);
        self.notify_ui();
    }

    fn notify_ui(&self) {
        if let Some(hwnd) = self.tray_window() {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_USAGE_UPDATED, WPARAM(0), LPARAM(0));
            }
        }
    }
}

fn window(handle: &AtomicIsize) -> Option<HWND> {
    let value = handle.load(Ordering::Relaxed);
    (value != 0).then(|| HWND(value as *mut _))
}

pub fn start_refresh_worker(state: Arc<SharedState>, receiver: mpsc::Receiver<RefreshCommand>) {
    thread::Builder::new()
        .name("usage-refresh".to_string())
        .spawn(move || {
            let mut previous_config: [Option<ProviderSettings>; Provider::COUNT] = [None, None];
            let mut command = RefreshCommand::RefreshAll;
            loop {
                let refresh_status = match command {
                    RefreshCommand::RefreshAll => true,
                    RefreshCommand::RefreshUsage => false,
                    RefreshCommand::Quit => break,
                };
                let kind = if refresh_status {
                    "status and usage"
                } else {
                    "usage-only"
                };
                diagnostics::event("INFO", &format!("{kind} refresh started"));
                state.refreshing.store(true, Ordering::Relaxed);
                state.notify_ui();
                for provider in Provider::ALL {
                    if refresh_status {
                        state.refresh_service_status(provider);
                    }
                    state.refresh_usage(provider, &mut previous_config[provider.index()]);
                }
                state.refreshing.store(false, Ordering::Relaxed);
                state.notify_ui();
                diagnostics::event("INFO", &format!("{kind} refresh finished"));

                command = match receiver.recv_timeout(REFRESH_INTERVAL) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => RefreshCommand::RefreshAll,
                    Err(mpsc::RecvTimeoutError::Disconnected) => RefreshCommand::Quit,
                };
            }
            diagnostics::event("INFO", "usage refresh worker stopped");
        })
        .expect("usage refresh worker could not be started");
}
