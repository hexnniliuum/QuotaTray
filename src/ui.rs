use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, GetDpiForWindow,
    MDT_EFFECTIVE_DPI, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::app::{SharedState, WM_USAGE_UPDATED};
use crate::model::{
    ExtraUsageBudget, Provider, ProviderSnapshot, ServiceStatus, ServiceStatusLevel, UsageWindow,
    format_countdown, now_unix,
};
use crate::palette::{self, Rgb};
use crate::settings::{self, Settings, Source};
use crate::startup;

const WM_TRAY: u32 = 0x8002;
const TRAY_CLASS: PCWSTR = w!("QuotaTray.MessageWindow");
const DASHBOARD_CLASS: PCWSTR = w!("QuotaTray.DashboardWindow");
const DASHBOARD_WIDTH: i32 = 380;
const CARD_HEADER_HEIGHT: i32 = 30;
const CARD_GAP: i32 = 7;
const HISTORY_ROW_HEIGHT: i32 = 20;
const WINDOW_ROW_HEIGHT: i32 = 52;
const MODEL_ROW_HEIGHT: i32 = 36;
const EXTRA_USAGE_ROW_HEIGHT: i32 = 28;
const MESSAGE_ROW_HEIGHT: i32 = 54;
const MENU_SOURCE_BASE: usize = 1;
const MENU_ADVANCED_SETTINGS: usize = MENU_SOURCE_BASE + Provider::COUNT * Source::ALL.len();

#[derive(Clone, Copy)]
struct Layout {
    scale: f32,
}

impl Layout {
    fn px(self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn rect(self, left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left: self.px(left),
            top: self.px(top),
            right: self.px(right),
            bottom: self.px(bottom),
        }
    }
}

#[derive(Clone, Copy)]
struct DashboardMetrics {
    provider_tops: [i32; Provider::COUNT],
    footer_top: i32,
    height: i32,
}

impl DashboardMetrics {
    fn from_state(state: &SharedState) -> Self {
        let mut provider_tops = [0; Provider::COUNT];
        let mut next_top = 58;
        for provider in Provider::ALL {
            if !state.is_provider_visible(provider) {
                continue;
            }
            provider_tops[provider.index()] = next_top;
            next_top += provider_height(&state.snapshot(provider));
        }
        let footer_top = next_top + 30;
        Self {
            provider_tops,
            footer_top,
            height: footer_top + 50,
        }
    }

    fn provider_top(self, provider: Provider) -> i32 {
        self.provider_tops[provider.index()]
    }

    fn refresh_rect(self, layout: Layout) -> RECT {
        layout.rect(282, 12, 364, 44)
    }

    fn startup_rect(self, layout: Layout) -> RECT {
        layout.rect(16, self.footer_top, 210, self.footer_top + 36)
    }

    fn provider_toggle_rect(self, provider: Provider, layout: Layout) -> RECT {
        let left = 16 + provider.index() as i32 * 116;
        layout.rect(left, self.footer_top - 26, left + 108, self.footer_top - 4)
    }

    fn sources_rect(self, layout: Layout) -> RECT {
        layout.rect(218, self.footer_top, 298, self.footer_top + 36)
    }

    fn exit_rect(self, layout: Layout) -> RECT {
        layout.rect(306, self.footer_top, 364, self.footer_top + 36)
    }
}

fn provider_height(snapshot: &ProviderSnapshot) -> i32 {
    let window_count =
        usize::from(snapshot.session.is_some()) + usize::from(snapshot.weekly.is_some());
    let has_usage =
        window_count > 0 || !snapshot.model_windows.is_empty() || snapshot.extra_usage.is_some();
    CARD_HEADER_HEIGHT
        + CARD_GAP
        + window_count as i32 * WINDOW_ROW_HEIGHT
        + snapshot.model_windows.len() as i32 * MODEL_ROW_HEIGHT
        + i32::from(snapshot.extra_usage.is_some()) * EXTRA_USAGE_ROW_HEIGHT
        + i32::from(snapshot.from_session_history) * HISTORY_ROW_HEIGHT
        + if !has_usage || snapshot.error.is_some() {
            MESSAGE_ROW_HEIGHT
        } else {
            0
        }
}

fn error_summary(error: &str) -> String {
    let prefix: String = error.chars().take(75).collect();
    format!(
        "{prefix}{} Click for details.",
        if error.chars().count() > 75 {
            "..."
        } else {
            ""
        }
    )
}

pub fn run(state: Arc<SharedState>) -> windows::core::Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        register_classes(instance)?;

        let state_pointer = Arc::into_raw(state.clone()) as *const c_void;
        let tray = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            TRAY_CLASS,
            w!("Quota Tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            Some(state_pointer),
        )?;
        state.set_tray_window(tray);
        state
            .start_with_windows
            .store(startup::is_enabled(), Ordering::Relaxed);
        update_tray_icons(tray, &state);

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe fn register_classes(instance: HINSTANCE) -> windows::core::Result<()> {
    let tray_class = WNDCLASSW {
        hInstance: instance,
        lpszClassName: TRAY_CLASS,
        lpfnWndProc: Some(tray_proc),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&tray_class) } == 0 {
        return Err(windows::core::Error::from_win32());
    }

    let dashboard_class = WNDCLASSW {
        hInstance: instance,
        lpszClassName: DASHBOARD_CLASS,
        lpfnWndProc: Some(dashboard_proc),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
        ..Default::default()
    };
    if unsafe { RegisterClassW(&dashboard_class) } == 0 {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

unsafe extern "system" fn tray_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
            LRESULT(1)
        }
        WM_TRAY => {
            match lparam.0 as u32 {
                WM_LBUTTONUP => {
                    if let Some(state) = state_from_window(hwnd) {
                        unsafe { toggle_dashboard(hwnd, state) };
                    }
                }
                WM_RBUTTONUP => {
                    if let Some(state) = state_from_window(hwnd) {
                        state.request_usage_refresh();
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_USAGE_UPDATED => {
            if let Some(state) = state_from_window(hwnd) {
                unsafe { update_tray_icons(hwnd, state) };
                if let Some(dashboard) = state.dashboard_window() {
                    unsafe {
                        resize_dashboard_to_content(dashboard, state);
                        let _ = InvalidateRect(Some(dashboard), None, false);
                    };
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(state) = state_from_window(hwnd) {
                unsafe { remove_tray_icons(hwnd, state) };
                state.request_quit();
            }
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const SharedState;
            if !pointer.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    drop(Arc::from_raw(pointer));
                }
            }
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

unsafe extern "system" fn dashboard_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize) };
            LRESULT(1)
        }
        WM_PAINT => {
            if let Some(state) = state_from_window(hwnd) {
                unsafe { paint_dashboard(hwnd, state) };
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = state_from_window(hwnd) {
                let layout = layout_for_window(hwnd);
                let metrics = DashboardMetrics::from_state(state);
                let x = (lparam.0 as i16) as i32;
                let y = ((lparam.0 >> 16) as i16) as i32;
                if point_in_rect(x, y, metrics.refresh_rect(layout)) {
                    state.request_refresh();
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    };
                } else if let Some(provider) = Provider::ALL.into_iter().find(|provider| {
                    point_in_rect(x, y, metrics.provider_toggle_rect(*provider, layout))
                }) {
                    if state.toggle_provider(provider) {
                        if let Some(tray) = state.tray_window() {
                            unsafe { update_tray_icons(tray, state) };
                        }
                        unsafe {
                            resize_dashboard_to_content(hwnd, state);
                            let _ = InvalidateRect(Some(hwnd), None, false);
                        };
                    }
                } else if point_in_rect(x, y, metrics.sources_rect(layout)) {
                    if let Err(error) = show_sources_menu(hwnd, state) {
                        crate::show_error(&error);
                    }
                } else if point_in_rect(x, y, metrics.startup_rect(layout)) {
                    let enabled = !state.start_with_windows.load(Ordering::Relaxed);
                    if startup::set_enabled(enabled).is_ok() {
                        state.start_with_windows.store(enabled, Ordering::Relaxed);
                    }
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    };
                } else if let Some(error) = Provider::ALL.into_iter().find_map(|provider| {
                    let snapshot = state.snapshot(provider);
                    let top = metrics.provider_top(provider);
                    (state.is_provider_visible(provider)
                        && point_in_rect(
                            x,
                            y,
                            layout.rect(16, top, 364, top + provider_height(&snapshot)),
                        ))
                    .then_some(snapshot.error)
                    .flatten()
                }) {
                    crate::show_error(&error);
                } else if point_in_rect(x, y, metrics.exit_rect(layout)) {
                    if let Some(tray) = state.tray_window() {
                        unsafe { PostMessageW(Some(tray), WM_CLOSE, WPARAM(0), LPARAM(0)).ok() };
                    }
                }
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                };
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_DPICHANGED => {
            if let Some(state) = state_from_window(hwnd) {
                unsafe {
                    resize_dashboard_to_content(hwnd, state);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                };
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn show_sources_menu(hwnd: HWND, state: &SharedState) -> Result<(), String> {
    let loaded = Settings::load();
    let mut settings = loaded.clone().unwrap_or_default();
    let menu = unsafe { CreatePopupMenu() }.map_err(|_| "Could not open Sources.".to_string())?;
    let result = (|| {
        for provider in Provider::ALL {
            for (index, source) in Source::ALL.into_iter().enumerate() {
                let label = crate::wide(format!("{}: {}", provider.name(), source.label()));
                let mut flags = MF_STRING;
                if loaded.is_err() {
                    flags |= MF_GRAYED;
                }
                if settings.providers[provider.index()].source == source {
                    flags |= MF_CHECKED;
                }
                unsafe {
                    AppendMenuW(
                        menu,
                        flags,
                        MENU_SOURCE_BASE + provider.index() * Source::ALL.len() + index,
                        PCWSTR(label.as_ptr()),
                    )
                }
                .map_err(|_| "Could not build Sources menu.".to_string())?;
            }
            unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, None) }.ok();
        }
        unsafe {
            AppendMenuW(
                menu,
                MF_STRING,
                MENU_ADVANCED_SETTINGS,
                w!("Edit advanced settings..."),
            )
        }
            .map_err(|_| "Could not build Sources menu.".to_string())?;
        let mut point = POINT::default();
        unsafe { GetCursorPos(&mut point) }
            .map_err(|_| "Cursor position unavailable.".to_string())?;
        let selected = unsafe {
            TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY,
                point.x,
                point.y,
                None,
                hwnd,
                None,
            )
        }
        .0 as usize;
        if (MENU_SOURCE_BASE..MENU_ADVANCED_SETTINGS).contains(&selected) {
            let choice = selected - MENU_SOURCE_BASE;
            let provider_index = choice / Source::ALL.len();
            settings.providers[provider_index].source = Source::ALL[choice % Source::ALL.len()];
            settings.save()?;
            state.request_usage_refresh();
        } else if selected == MENU_ADVANCED_SETTINGS {
            let path = settings::settings_path()?;
            if !path.exists() {
                settings.save()?;
            }
            std::process::Command::new("notepad.exe")
                .arg(path)
                .spawn()
                .map_err(|_| "Could not open sources.json in Notepad.".to_string())?;
        }
        Ok(())
    })();
    unsafe { DestroyMenu(menu) }.ok();
    result
}

fn state_from_window(hwnd: HWND) -> Option<&'static SharedState> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const SharedState;
    unsafe { pointer.as_ref() }
}

unsafe fn update_tray_icons(hwnd: HWND, state: &SharedState) {
    let mut handles = state.icon_handles.lock().unwrap();
    for (index, provider) in Provider::ALL.into_iter().enumerate() {
        if state.is_provider_visible(provider) {
            let (icon, tooltip) =
                create_usage_icon_and_tip(state.snapshot(provider), state.service_status(provider));
            let data = notification_data(hwnd, index as u32 + 1, icon, &tooltip);
            let operation = if handles[index].is_some() {
                NIM_MODIFY
            } else {
                NIM_ADD
            };
            unsafe {
                let _ = Shell_NotifyIconW(operation, &data);
            };
            if let Some(previous) = handles[index].replace(icon.0 as isize) {
                unsafe { DestroyIcon(HICON(previous as *mut _)).ok() };
            }
        } else if let Some(previous) = handles[index].take() {
            let data = NOTIFYICONDATAW {
                cbSize: size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: index as u32 + 1,
                ..Default::default()
            };
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                DestroyIcon(HICON(previous as *mut _)).ok();
            }
        }
    }
}

unsafe fn remove_tray_icons(hwnd: HWND, state: &SharedState) {
    for id in 1..=Provider::COUNT as u32 {
        let data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: id,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        };
    }
    for icon in state.icon_handles.lock().unwrap().iter_mut() {
        if let Some(handle) = icon.take() {
            unsafe { DestroyIcon(HICON(handle as *mut _)).ok() };
        }
    }
}

fn notification_data(hwnd: HWND, id: u32, icon: HICON, tooltip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: id,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon,
        ..Default::default()
    };
    copy_wide_fixed(tooltip, &mut data.szTip);
    data
}

fn create_usage_icon_and_tip(
    snapshot: ProviderSnapshot,
    service_status: ServiceStatus,
) -> (HICON, String) {
    let now = now_unix();
    let displayed = snapshot.displayed_window(now);
    let text = displayed
        .map(|window| format!("{:.0}", window.used_percent))
        .unwrap_or_else(|| "--".to_string());
    let color = displayed
        .map(|window| palette::usage_color(snapshot.provider, window.used_percent))
        .unwrap_or(palette::TRACK);
    let tooltip = if let Some(window) = displayed {
        format!(
            "{} · {} · {} {:.0}% · {}",
            snapshot.provider.name(),
            service_status.label(),
            window.label,
            window.used_percent,
            format_countdown(window.resets_at_unix, now)
        )
    } else {
        format!(
            "{} · {} · usage unavailable",
            snapshot.provider.name(),
            service_status.label()
        )
    };
    (
        create_circle_icon(
            &text,
            displayed.map_or(0.0, |window| window.used_percent),
            color,
            service_status.level,
        ),
        tooltip,
    )
}

fn create_circle_icon(
    text: &str,
    percentage: f64,
    color: Rgb,
    service_status: ServiceStatusLevel,
) -> HICON {
    const SIZE: i32 = 64;
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        let Ok(color_bitmap) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            return HICON::default();
        };
        if bits.is_null() {
            let _ = DeleteObject(color_bitmap.into());
            return HICON::default();
        }
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
        render_circle_pixels(pixels, SIZE, percentage, color, service_status);

        let dc = CreateCompatibleDC(None);
        let old_bitmap = SelectObject(dc, color_bitmap.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, colorref(Rgb(255, 255, 255)));
        let font_height = if text.len() >= 3 { -24 } else { -31 };
        let font = CreateFontW(
            font_height,
            0,
            0,
            0,
            FW_HEAVY.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let old_font = SelectObject(dc, font.into());
        let mut text_wide = text.encode_utf16().collect::<Vec<_>>();
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: SIZE,
            bottom: SIZE,
        };
        DrawTextW(
            dc,
            &mut text_wide,
            &mut rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        SelectObject(dc, old_font);
        SelectObject(dc, old_bitmap);
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
        for pixel in pixels {
            if *pixel & 0x00ff_ffff != 0 && *pixel & 0xff00_0000 == 0 {
                *pixel |= 0xff00_0000;
            }
        }
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);

        let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
        let icon_info = ICONINFO {
            fIcon: TRUE,
            hbmColor: color_bitmap,
            hbmMask: mask,
            ..Default::default()
        };
        let icon = CreateIconIndirect(&icon_info).unwrap_or_default();
        let _ = DeleteObject(color_bitmap.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn render_circle_pixels(
    pixels: &mut [u32],
    size: i32,
    percentage: f64,
    color: Rgb,
    service_status: ServiceStatusLevel,
) {
    let center = (size as f64 - 1.0) / 2.0;
    let ring_outer = size as f64 * 0.50;
    let ring_inner = size as f64 * 0.37;
    let glow_inner = size as f64 * 0.14;
    let fill_angle = percentage.clamp(0.0, 100.0) / 100.0 * std::f64::consts::TAU;
    let glow = status_inner_glow(service_status);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f64 - center;
            let dy = y as f64 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let pixel = if distance <= ring_inner {
                let background = glow.map_or(palette::BACKGROUND, |(glow_color, strength)| {
                    if distance <= glow_inner {
                        palette::BACKGROUND
                    } else {
                        let progress = (distance - glow_inner) / (ring_inner - glow_inner);
                        blend(
                            palette::BACKGROUND,
                            glow_color,
                            strength * progress.powf(1.35),
                        )
                    }
                });
                bgra(background)
            } else if distance <= ring_outer {
                let angle = dx.atan2(-dy).rem_euclid(std::f64::consts::TAU);
                bgra(if angle <= fill_angle {
                    color
                } else {
                    palette::TRACK
                })
            } else {
                0
            };
            pixels[(y * size + x) as usize] = pixel;
        }
    }
}

fn status_inner_glow(level: ServiceStatusLevel) -> Option<(Rgb, f64)> {
    let strength = match level {
        ServiceStatusLevel::Operational | ServiceStatusLevel::Unavailable => return None,
        ServiceStatusLevel::Degraded | ServiceStatusLevel::Maintenance => 0.21,
        ServiceStatusLevel::PartialOutage => 0.32,
        ServiceStatusLevel::MajorOutage => 0.46,
    };
    Some((palette::service_status_color(level), strength))
}

fn blend(background: Rgb, foreground: Rgb, amount: f64) -> Rgb {
    let amount = amount.clamp(0.0, 1.0);
    let channel = |background: u8, foreground: u8| {
        (f64::from(background) * (1.0 - amount) + f64::from(foreground) * amount).round() as u8
    };
    Rgb(
        channel(background.0, foreground.0),
        channel(background.1, foreground.1),
        channel(background.2, foreground.2),
    )
}

unsafe fn create_dashboard(owner: HWND, state: &SharedState) -> Option<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let pointer = state as *const SharedState;
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            DASHBOARD_CLASS,
            w!("Quota Tray"),
            WS_POPUP | WS_BORDER,
            0,
            0,
            0,
            0,
            Some(owner),
            None,
            Some(HINSTANCE(module.0)),
            Some(pointer.cast()),
        )
    }
    .ok()
}

unsafe fn toggle_dashboard(owner: HWND, state: &SharedState) {
    let dashboard = match state.dashboard_window() {
        Some(dashboard) => dashboard,
        None => {
            let Some(created) = (unsafe { create_dashboard(owner, state) }) else {
                return;
            };
            state.set_dashboard_window(created);
            created
        }
    };
    if unsafe { IsWindowVisible(dashboard).as_bool() } {
        unsafe {
            let _ = ShowWindow(dashboard, SW_HIDE);
        };
        return;
    }

    let mut cursor = POINT::default();
    unsafe { GetCursorPos(&mut cursor).ok() };
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    let mut monitor_info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mut dpi = 96u32;
    let mut dpi_y = 96u32;
    unsafe {
        let _ = GetMonitorInfoW(monitor, &mut monitor_info);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y);
    };
    let layout = Layout {
        scale: dpi.max(96) as f32 / 96.0,
    };
    let metrics = DashboardMetrics::from_state(state);
    let width = layout.px(DASHBOARD_WIDTH);
    let height = layout.px(metrics.height);
    let x = (cursor.x - width / 2)
        .max(monitor_info.rcWork.left)
        .min(monitor_info.rcWork.right - width);
    let y = (cursor.y - height - layout.px(8))
        .max(monitor_info.rcWork.top)
        .min(monitor_info.rcWork.bottom - height);
    unsafe {
        SetWindowPos(
            dashboard,
            Some(HWND_TOPMOST),
            x,
            y,
            width,
            height,
            SWP_SHOWWINDOW,
        )
        .ok();
        let _ = SetForegroundWindow(dashboard);
        let _ = InvalidateRect(Some(dashboard), None, false);
    }
}

unsafe fn resize_dashboard_to_content(hwnd: HWND, state: &SharedState) {
    if !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return;
    }
    let layout = layout_for_window(hwnd);
    let width = layout.px(DASHBOARD_WIDTH);
    let height = layout.px(DashboardMetrics::from_state(state).height);
    let mut current = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut current) }.is_err() {
        return;
    }
    if current.right - current.left == width && current.bottom - current.top == height {
        return;
    }
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            current.left,
            current.bottom - height,
            width,
            height,
            SWP_NOACTIVATE | SWP_NOZORDER,
        )
        .ok();
    }
}

unsafe fn paint_dashboard(hwnd: HWND, state: &SharedState) {
    let mut paint = PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let layout = layout_for_window(hwnd);
    let metrics = DashboardMetrics::from_state(state);
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client).ok() };
    fill_rect(dc, client, palette::BACKGROUND);
    unsafe { SetBkMode(dc, TRANSPARENT) };

    draw_text(
        dc,
        layout.rect(16, 12, 250, 42),
        "Usage",
        22,
        FW_BOLD.0 as i32,
        palette::TEXT,
        DT_LEFT,
    );
    let refreshing = state.refreshing.load(Ordering::Relaxed);
    draw_button(
        dc,
        metrics.refresh_rect(layout),
        if refreshing { "Working" } else { "Refresh" },
    );

    for provider in Provider::ALL {
        if !state.is_provider_visible(provider) {
            continue;
        }
        paint_provider(
            dc,
            state.snapshot(provider),
            state.service_status(provider),
            metrics.provider_top(provider),
            layout,
        );
    }

    for provider in Provider::ALL {
        let marker = if state.is_provider_visible(provider) {
            "✓ "
        } else {
            ""
        };
        draw_text(
            dc,
            metrics.provider_toggle_rect(provider, layout),
            &format!("{marker}{}", provider.name()),
            11,
            FW_NORMAL.0 as i32,
            if state.is_provider_visible(provider) {
                palette::TEXT
            } else {
                palette::MUTED_TEXT
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }

    let startup_label = if state.start_with_windows.load(Ordering::Relaxed) {
        "✓ Start with Windows"
    } else {
        "Start with Windows"
    };
    draw_button(dc, metrics.startup_rect(layout), startup_label);
    draw_button(dc, metrics.sources_rect(layout), "Sources");
    draw_button(dc, metrics.exit_rect(layout), "Exit");
    unsafe {
        let _ = EndPaint(hwnd, &paint);
    };
}

fn paint_provider(
    dc: HDC,
    snapshot: ProviderSnapshot,
    service_status: ServiceStatus,
    top: i32,
    layout: Layout,
) {
    let now = now_unix();
    let status_left = match snapshot.provider {
        Provider::Claude => 78,
        Provider::Codex => 70,
    };
    draw_text(
        dc,
        layout.rect(16, top, status_left - 2, top + 25),
        snapshot.provider.name(),
        16,
        FW_BOLD.0 as i32,
        palette::TEXT,
        DT_LEFT,
    );
    draw_text(
        dc,
        layout.rect(status_left, top, 242, top + 25),
        service_status.label(),
        11,
        FW_NORMAL.0 as i32,
        palette::service_status_color(service_status.level),
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    if !snapshot.from_session_history {
        draw_text(
            dc,
            layout.rect(242, top, 364, top + 25),
            &if snapshot.error.is_some() {
                "Refresh failed".into()
            } else {
                snapshot.freshness_label(now)
            },
            12,
            FW_NORMAL.0 as i32,
            palette::MUTED_TEXT,
            DT_RIGHT,
        );
    }
    let mut window_top = top + CARD_HEADER_HEIGHT;
    if snapshot.from_session_history {
        draw_text(
            dc,
            layout.rect(16, window_top, 364, window_top + HISTORY_ROW_HEIGHT),
            &snapshot.freshness_label(now),
            11,
            FW_NORMAL.0 as i32,
            palette::MUTED_TEXT,
            DT_LEFT | DT_SINGLELINE,
        );
        window_top += HISTORY_ROW_HEIGHT;
    }
    let windows_start = window_top;
    for window in [snapshot.session.as_ref(), snapshot.weekly.as_ref()]
        .into_iter()
        .flatten()
    {
        paint_window(dc, snapshot.provider, window, window_top, layout);
        draw_text(
            dc,
            layout.rect(16, window_top + 34, 364, window_top + 51),
            &format_countdown(window.resets_at_unix, now),
            11,
            FW_NORMAL.0 as i32,
            palette::MUTED_TEXT,
            DT_LEFT,
        );
        window_top += WINDOW_ROW_HEIGHT;
    }
    for window in &snapshot.model_windows {
        paint_window(dc, snapshot.provider, window, window_top, layout);
        window_top += MODEL_ROW_HEIGHT;
    }
    if let Some(budget) = snapshot.extra_usage.as_ref() {
        paint_extra_usage(dc, snapshot.provider, budget, window_top, layout);
        window_top += EXTRA_USAGE_ROW_HEIGHT;
    }
    if window_top == windows_start || snapshot.error.is_some() {
        let error = snapshot
            .error
            .as_deref()
            .map(error_summary)
            .unwrap_or_else(|| "Waiting for usage...".into());
        draw_text(
            dc,
            layout.rect(16, window_top, 364, window_top + MESSAGE_ROW_HEIGHT),
            &error,
            12,
            FW_NORMAL.0 as i32,
            palette::MUTED_TEXT,
            DT_LEFT | DT_WORDBREAK,
        );
    }
}

fn paint_window(dc: HDC, provider: Provider, window: &UsageWindow, top: i32, layout: Layout) {
    let applicable = window.is_applicable(now_unix());
    let percentage = if applicable { window.used_percent } else { 0.0 };
    let label = if applicable {
        window.label.as_str()
    } else {
        "Expired window"
    };
    draw_text(
        dc,
        layout.rect(16, top, 250, top + 20),
        label,
        12,
        FW_NORMAL.0 as i32,
        palette::TEXT,
        DT_LEFT,
    );
    draw_text(
        dc,
        layout.rect(250, top, 364, top + 20),
        &format!("{:.0}%", percentage),
        14,
        FW_BOLD.0 as i32,
        palette::usage_color(provider, percentage),
        DT_RIGHT,
    );
    let track = layout.rect(16, top + 23, 364, top + 30);
    fill_rect(dc, track, palette::TRACK);
    let mut fill = track;
    fill.right = fill.left + ((fill.right - fill.left) as f64 * percentage / 100.0) as i32;
    fill_rect(dc, fill, palette::usage_color(provider, percentage));
}

fn paint_extra_usage(
    dc: HDC,
    provider: Provider,
    budget: &ExtraUsageBudget,
    top: i32,
    layout: Layout,
) {
    let right_label = budget
        .remaining_minor()
        .zip(budget.limit_minor)
        .map(|(remaining, limit)| {
            format!(
                "{} left of {}",
                budget.format_amount(remaining),
                budget.format_amount(limit)
            )
        })
        .unwrap_or_else(|| format!("{} spent · no cap", budget.format_amount(budget.used_minor)));
    draw_text(
        dc,
        layout.rect(16, top, 180, top + 22),
        "Extra usage",
        12,
        FW_NORMAL.0 as i32,
        palette::TEXT,
        DT_LEFT,
    );
    draw_text(
        dc,
        layout.rect(180, top, 364, top + 22),
        &right_label,
        12,
        FW_BOLD.0 as i32,
        budget
            .used_percent()
            .map(|value| palette::usage_color(provider, value))
            .unwrap_or(palette::MUTED_TEXT),
        DT_RIGHT,
    );
}

fn draw_button(dc: HDC, rect: RECT, label: &str) {
    fill_rect(dc, rect, Rgb(52, 52, 56));
    draw_text(
        dc,
        rect,
        label,
        12,
        FW_NORMAL.0 as i32,
        palette::TEXT,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

fn draw_text(
    dc: HDC,
    mut rect: RECT,
    value: &str,
    size: i32,
    weight: i32,
    color: Rgb,
    alignment: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let font_height = -((size * GetDeviceCaps(Some(dc), LOGPIXELSY) + 48) / 96).max(1);
        let font = CreateFontW(
            font_height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        let old = SelectObject(dc, font.into());
        SetTextColor(dc, colorref(color));
        let mut text = value.encode_utf16().collect::<Vec<_>>();
        DrawTextW(dc, &mut text, &mut rect, alignment | DT_NOPREFIX);
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
    }
}

fn fill_rect(dc: HDC, rect: RECT, color: Rgb) {
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        FillRect(dc, &rect, brush);
        let _ = DeleteObject(brush.into());
    }
}

fn layout_for_window(hwnd: HWND) -> Layout {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    Layout {
        scale: dpi as f32 / 96.0,
    }
}

fn point_in_rect(x: i32, y: i32, rect: RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn colorref(Rgb(red, green, blue): Rgb) -> COLORREF {
    COLORREF(red as u32 | ((green as u32) << 8) | ((blue as u32) << 16))
}

fn bgra(Rgb(red, green, blue): Rgb) -> u32 {
    ((255u32) << 24) | ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

fn copy_wide_fixed<const N: usize>(value: &str, target: &mut [u16; N]) {
    for (destination, source) in target.iter_mut().zip(value.encode_utf16().chain(Some(0))) {
        *destination = source;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(pixel: u32) -> u32 {
        pixel & 0x00ff_ffff
    }

    #[test]
    fn long_errors_keep_the_card_bounded_and_offer_full_details() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Claude);
        snapshot.error = Some("Missing login. ".repeat(100));
        assert_eq!(provider_height(&snapshot), 91);
        let summary = error_summary(snapshot.error.as_ref().unwrap());
        assert!(summary.len() < 110);
        assert!(summary.ends_with("Click for details."));
        snapshot.session = Some(UsageWindow::new("Session", 10.0, None));
        assert_eq!(provider_height(&snapshot), 143);
    }

    #[test]
    fn model_usage_window_adds_room_for_a_separate_bar() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Claude);
        snapshot.session = Some(UsageWindow::new("Session", 10.0, None));
        snapshot.weekly = Some(UsageWindow::new("Weekly", 20.0, None));
        let without_model_window = provider_height(&snapshot);

        snapshot
            .model_windows
            .push(UsageWindow::new("Fable 5", 30.0, None));

        assert_eq!(provider_height(&snapshot), without_model_window + 36);
    }

    #[test]
    fn operational_icons_keep_the_original_background() {
        let mut pixels = vec![0; 64 * 64];
        render_circle_pixels(
            &mut pixels,
            64,
            50.0,
            Rgb(255, 255, 255),
            ServiceStatusLevel::Operational,
        );

        assert_eq!(pixels[32 * 64 + 54], bgra(palette::BACKGROUND));
    }

    #[test]
    fn outage_glow_stays_inside_the_usage_ring_and_away_from_text() {
        let mut pixels = vec![0; 64 * 64];
        render_circle_pixels(
            &mut pixels,
            64,
            50.0,
            Rgb(255, 255, 255),
            ServiceStatusLevel::PartialOutage,
        );

        assert_eq!(pixels[32 * 64 + 32], bgra(palette::BACKGROUND));
        assert_ne!(pixels[32 * 64 + 46], bgra(palette::BACKGROUND));
        assert_ne!(pixels[32 * 64 + 54], bgra(palette::BACKGROUND));
        assert_eq!(pixels[0], 0);
    }

    #[test]
    fn major_outage_inner_glow_is_stronger_than_partial_outage() {
        let mut partial = vec![0; 64 * 64];
        let mut major = vec![0; 64 * 64];
        render_circle_pixels(
            &mut partial,
            64,
            50.0,
            Rgb(255, 255, 255),
            ServiceStatusLevel::PartialOutage,
        );
        render_circle_pixels(
            &mut major,
            64,
            50.0,
            Rgb(255, 255, 255),
            ServiceStatusLevel::MajorOutage,
        );

        let background = rgb(bgra(palette::BACKGROUND));
        let color_distance = |pixel: u32| rgb(pixel).abs_diff(background);
        assert!(color_distance(major[32 * 64 + 54]) > color_distance(partial[32 * 64 + 54]));
    }

    #[test]
    fn service_status_does_not_recolor_the_usage_ring() {
        let mut pixels = vec![0; 64 * 64];
        let usage_color = Rgb(12, 34, 56);
        render_circle_pixels(
            &mut pixels,
            64,
            100.0,
            usage_color,
            ServiceStatusLevel::MajorOutage,
        );

        assert_eq!(pixels[32 * 64 + 58], bgra(usage_color));
    }
}
