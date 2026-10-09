mod animation;
mod drawing;
mod layout;
mod resets;

use std::cell::RefCell;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWM_WINDOW_CORNER_PREFERENCE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
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
use crate::palette::{self, Rgb, TrayTheme, UsageColors, blend};
use crate::pulse::{self, Frame, Trace};
use crate::settings::{self, Settings, Source};
use crate::startup;

use animation::{DashboardAnimation, PULSE_TIMER, PulseClock};
use drawing::{
    Font, Layout, bgra, colorref, draw_polyline, draw_text, fill_ellipse, fill_rect,
    fill_round_rect, measure_text,
};
use layout::{CardLayout, CardRow};

const WM_TRAY: u32 = 0x8002;
static WM_TASKBAR_CREATED: LazyLock<u32> =
    LazyLock::new(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) });
const TRAY_CLASS: PCWSTR = w!("QuotaTray.MessageWindow");
const DASHBOARD_CLASS: PCWSTR = w!("QuotaTray.DashboardWindow");
const DASHBOARD_WIDTH: i32 = 380;
const CONTENT_LEFT: i32 = 16;
const CONTENT_RIGHT: i32 = 364;
const CARD_LEFT: i32 = 30;
const CARD_RIGHT: i32 = 350;
const CARD_PAD_TOP: i32 = 12;
const CARD_PAD_BOTTOM: i32 = 10;
const CARD_HEADER_HEIGHT: i32 = 30;
const CARD_GAP: i32 = 8;
const CARD_RADIUS: i32 = 10;
const HISTORY_ROW_HEIGHT: i32 = 20;
const WINDOW_ROW_HEIGHT: i32 = 46;
const MODEL_ROW_HEIGHT: i32 = 30;
const EXTRA_USAGE_ROW_HEIGHT: i32 = 30;
const RESET_ROW_HEIGHT: i32 = 20;
const MESSAGE_ROW_HEIGHT: i32 = 54;
const FOOTER_HEIGHT: i32 = 40;
/// Right edge of the usage bars; the percentage and reset columns follow.
const BARS_RIGHT: i32 = 224;
const MENU_SOURCE_BASE: usize = 1;
const MENU_ADVANCED_SETTINGS: usize = MENU_SOURCE_BASE + Provider::COUNT * Source::ALL.len();
const MENU_APPEARANCE_ERROR: usize = MENU_ADVANCED_SETTINGS + 1;

#[derive(Clone, Copy)]
struct DashboardMetrics {
    provider_tops: [i32; Provider::COUNT],
    footer_top: i32,
    height: i32,
}

impl DashboardMetrics {
    fn from_state(state: &SharedState) -> Self {
        let mut provider_tops = [0; Provider::COUNT];
        let mut next_top = 58 + if state.notice().is_some() { 68 } else { 0 };
        for provider in Provider::ALL {
            if !state.is_provider_visible(provider) {
                continue;
            }
            provider_tops[provider.index()] = next_top;
            next_top += CardLayout::new(&state.snapshot(provider)).height;
        }
        let footer_top = next_top + 4;
        Self {
            provider_tops,
            footer_top,
            height: footer_top + FOOTER_HEIGHT,
        }
    }

    fn provider_top(self, provider: Provider) -> i32 {
        self.provider_tops[provider.index()]
    }

    fn refresh_rect(self, layout: Layout) -> RECT {
        layout.rect(292, 16, CONTENT_RIGHT, 44)
    }

    fn theme_rect(self, layout: Layout) -> RECT {
        layout.rect(252, 16, 280, 44)
    }

    fn startup_rect(self, layout: Layout) -> RECT {
        layout.rect(140, self.footer_top + 4, 270, self.footer_top + 28)
    }

    fn provider_toggle_rect(self, provider: Provider, layout: Layout) -> RECT {
        let left = CONTENT_LEFT + provider.index() as i32 * 62;
        layout.rect(left, self.footer_top + 4, left + 56, self.footer_top + 28)
    }

    fn sources_rect(self, layout: Layout) -> RECT {
        layout.rect(278, self.footer_top + 4, 330, self.footer_top + 28)
    }

    fn exit_rect(self, layout: Layout) -> RECT {
        layout.rect(
            338,
            self.footer_top + 4,
            CONTENT_RIGHT,
            self.footer_top + 28,
        )
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

        let tray = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            TRAY_CLASS,
            w!("Quota Tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            Some((&state as *const Arc<SharedState>).cast()),
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
            attach_window_state(hwnd, lparam);
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
                let generation = state.refresh_generation.load(Ordering::Relaxed);
                if let Some(dashboard) = state.dashboard_window() {
                    unsafe {
                        if let Some(window) = window_state(dashboard) {
                            window
                                .animation
                                .borrow_mut()
                                .on_refresh(dashboard, generation);
                        }
                        resize_dashboard_to_content(dashboard, state);
                        let _ = InvalidateRect(Some(dashboard), None, false);
                    };
                }
            }
            LRESULT(0)
        }
        message if message != 0 && message == *WM_TASKBAR_CREATED => {
            if let Some(state) = state_from_window(hwnd) {
                unsafe {
                    remove_tray_icons(hwnd, state);
                    update_tray_icons(hwnd, state);
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
            detach_window_state(hwnd);
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
            attach_window_state(hwnd, lparam);
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
                let codex = state.snapshot(Provider::Codex);
                let reset_hit = state.is_provider_visible(Provider::Codex)
                    && CardLayout::new(&codex).rows.iter().any(|row| {
                        matches!(row.content, CardRow::Resets)
                            && point_in_rect(
                                x,
                                y,
                                reset_rect(layout, metrics.provider_top(Provider::Codex) + row.top),
                            )
                    });
                if reset_hit {
                    if let Some(window) = window_state(hwnd) {
                        resets::open(window.shared.clone());
                    }
                } else if point_in_rect(x, y, metrics.refresh_rect(layout)) {
                    // The worker announces the refresh start, which begins the trace.
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
                } else if point_in_rect(x, y, metrics.theme_rect(layout)) {
                    if let Err(error) = state.toggle_tray_theme() {
                        state.report_error(&error);
                    }
                    unsafe {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                } else if point_in_rect(x, y, metrics.sources_rect(layout)) {
                    if let Err(error) = show_sources_menu(hwnd, state) {
                        state.report_error(&error);
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
                            layout.rect(
                                CONTENT_LEFT,
                                top,
                                CONTENT_RIGHT,
                                top + CardLayout::new(&snapshot).height - CARD_GAP,
                            ),
                        ))
                    .then_some(snapshot.error)
                    .flatten()
                }) {
                    state.report_error(&error);
                } else if point_in_rect(x, y, metrics.exit_rect(layout))
                    && let Some(tray) = state.tray_window()
                {
                    unsafe { PostMessageW(Some(tray), WM_CLOSE, WPARAM(0), LPARAM(0)).ok() };
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == PULSE_TIMER {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                };
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                };
                if let Some(window) = window_state(hwnd) {
                    window.animation.borrow_mut().stop(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if let Some(state) = state_from_window(hwnd) {
                state.set_dashboard_window(HWND::default());
            }
            detach_window_state(hwnd);
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
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
        if settings.appearance.is_err() {
            unsafe {
                AppendMenuW(
                    menu,
                    MF_STRING,
                    MENU_APPEARANCE_ERROR,
                    w!("Appearance settings error..."),
                )
            }
            .map_err(|_| "Could not build Sources menu.".to_string())?;
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
        } else if selected == MENU_APPEARANCE_ERROR {
            if let Err(error) = &settings.appearance {
                return Err(format!(
                    "{error}\n\nOpen Sources > Edit advanced settings to fix it. Usage sources are unchanged."
                ));
            }
        } else if selected == MENU_ADVANCED_SETTINGS {
            let path = settings::settings_path()?;
            if !path.exists() {
                settings.save()?;
            }
            // A packaged launcher can redirect AppData. Notepad needs the
            // physical file path because it runs outside that package context.
            let path = path
                .canonicalize()
                .map_err(|_| "Could not resolve config.json for editing.".to_string())?;
            std::process::Command::new("notepad.exe")
                .arg(path)
                .spawn()
                .map_err(|_| "Could not open config.json in Notepad.".to_string())?;
        }
        Ok(())
    })();
    unsafe { DestroyMenu(menu) }.ok();
    result
}

struct WindowState {
    shared: Arc<SharedState>,
    animation: RefCell<DashboardAnimation>,
}

fn attach_window_state(hwnd: HWND, lparam: LPARAM) {
    let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
    let shared = unsafe { &*(create.lpCreateParams as *const Arc<SharedState>) };
    let state = Box::new(WindowState {
        shared: shared.clone(),
        animation: RefCell::new(DashboardAnimation::new(
            shared.refresh_generation.load(Ordering::Relaxed),
        )),
    });
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize) };
}

fn detach_window_state(hwnd: HWND) {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    if !pointer.is_null() {
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(pointer));
        }
    }
}

fn window_state(hwnd: HWND) -> Option<&'static WindowState> {
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    unsafe { pointer.as_ref() }
}

fn state_from_window(hwnd: HWND) -> Option<&'static SharedState> {
    window_state(hwnd).map(|window| window.shared.as_ref())
}

unsafe fn update_tray_icons(hwnd: HWND, state: &SharedState) {
    let mut handles = state.icon_handles.lock().unwrap();
    for (index, provider) in Provider::ALL.into_iter().enumerate() {
        if state.is_provider_visible(provider) {
            let (icon, tooltip) = create_usage_icon_and_tip(
                state.snapshot(provider),
                state.service_status(provider),
                state.usage_colors(provider),
                state.tray_theme(),
            );
            let data = notification_data(hwnd, index as u32 + 1, icon, &tooltip);
            let operation = if handles[index].is_some() {
                NIM_MODIFY
            } else {
                NIM_ADD
            };
            unsafe {
                if !Shell_NotifyIconW(operation, &data).as_bool() && operation == NIM_MODIFY {
                    let _ = Shell_NotifyIconW(NIM_ADD, &data);
                }
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
    colors: UsageColors,
    theme: TrayTheme,
) -> (HICON, String) {
    let now = now_unix();
    let displayed = snapshot.displayed_window(now);
    let text = displayed
        .map(|window| format!("{:.0}", window.used_percent))
        .unwrap_or_else(|| "--".to_string());
    let color = displayed
        .map(|window| theme.usage_color(colors.at(window.used_percent)))
        .unwrap_or(theme.track());
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
            theme,
        ),
        tooltip,
    )
}

fn create_circle_icon(
    text: &str,
    percentage: f64,
    color: Rgb,
    service_status: ServiceStatusLevel,
    theme: TrayTheme,
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
        render_circle_pixels(pixels, SIZE, percentage, color, service_status, theme);

        let dc = CreateCompatibleDC(None);
        let old_bitmap = SelectObject(dc, color_bitmap.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, colorref(theme.text()));
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
    theme: TrayTheme,
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
                let background = glow.map_or(theme.background(), |(glow_color, strength)| {
                    if distance <= glow_inner {
                        theme.background()
                    } else {
                        let progress = (distance - glow_inner) / (ring_inner - glow_inner);
                        blend(
                            theme.background(),
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
                    theme.track()
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

unsafe fn create_dashboard(owner: HWND) -> Option<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.ok()?;
    let shared = &window_state(owner)?.shared;
    let hwnd = unsafe {
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
            Some((shared as *const Arc<SharedState>).cast()),
        )
    }
    .ok()?;
    let corners = DWMWCP_ROUND;
    unsafe {
        // Windows 11 rounds the popup; older versions ignore the request.
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&corners as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
    }
    Some(hwnd)
}

unsafe fn toggle_dashboard(owner: HWND, state: &SharedState) {
    let dashboard = match state.dashboard_window() {
        Some(dashboard) => dashboard,
        None => {
            let Some(created) = (unsafe { create_dashboard(owner) }) else {
                return;
            };
            state.set_dashboard_window(created);
            created
        }
    };
    if unsafe { IsWindowVisible(dashboard).as_bool() } {
        unsafe {
            let _ = ShowWindow(dashboard, SW_HIDE);
            if let Some(window) = window_state(dashboard) {
                window.animation.borrow_mut().stop(dashboard);
            }
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
        theme: state.tray_theme(),
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

/// Where the next usage bar sits in the pulse sequence and whether any bar is
/// still drawing.
struct PulseCursor {
    clock: Option<PulseClock>,
    next_bar: usize,
    animating: bool,
}

unsafe fn paint_dashboard(hwnd: HWND, state: &SharedState) {
    let mut paint = PAINTSTRUCT::default();
    let dc = unsafe { BeginPaint(hwnd, &mut paint) };
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client).ok() };
    let width = (client.right - client.left).max(1);
    let height = (client.bottom - client.top).max(1);
    // Draw into an off-screen bitmap so the animation never flickers.
    let buffer = unsafe { CreateCompatibleDC(Some(dc)) };
    let bitmap = unsafe { CreateCompatibleBitmap(dc, width, height) };
    let previous = unsafe { SelectObject(buffer, bitmap.into()) };
    let animating = render_dashboard(buffer, client, hwnd, state);
    unsafe {
        let _ = BitBlt(dc, 0, 0, width, height, Some(buffer), 0, 0, SRCCOPY);
        SelectObject(buffer, previous);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(buffer);
        let _ = EndPaint(hwnd, &paint);
    }
    if let Some(window) = window_state(hwnd) {
        window.animation.borrow_mut().finish_frame(hwnd, animating);
    }
}

/// Paints the whole dashboard; returns whether a refresh trace is still running.
fn render_dashboard(dc: HDC, client: RECT, hwnd: HWND, state: &SharedState) -> bool {
    let layout = layout_for_window(hwnd);
    let metrics = DashboardMetrics::from_state(state);
    fill_rect(dc, client, layout.colors().background);
    unsafe { SetBkMode(dc, TRANSPARENT) };

    draw_text(
        dc,
        layout,
        layout.rect(CONTENT_LEFT, 16, 200, 44),
        "Usage",
        Font::semibold(17),
        layout.colors().text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let refreshing = state.refreshing.load(Ordering::Relaxed);
    draw_button(
        dc,
        metrics.refresh_rect(layout),
        if refreshing { "Working…" } else { "Refresh" },
        layout,
    );

    draw_button(
        dc,
        metrics.theme_rect(layout),
        if state.tray_theme() == TrayTheme::Dark {
            "☀"
        } else {
            "☾"
        },
        layout,
    );

    if let Some(notice) = state.notice() {
        fill_round_rect(
            dc,
            layout.rect(CONTENT_LEFT, 58, CONTENT_RIGHT, 120),
            layout.px(8),
            layout.colors().card,
        );
        draw_text(
            dc,
            layout,
            layout.rect(CARD_LEFT, 65, CARD_RIGHT, 116),
            &notice,
            Font::regular(12),
            layout.colors().text,
            DT_LEFT | DT_WORDBREAK | DT_END_ELLIPSIS,
        );
    }

    let mut cursor = PulseCursor {
        clock: window_state(hwnd).and_then(|window| window.animation.borrow().frame()),
        next_bar: 0,
        animating: false,
    };
    for provider in Provider::ALL {
        if !state.is_provider_visible(provider) {
            continue;
        }
        paint_provider(
            dc,
            state.snapshot(provider),
            state.service_status(provider),
            state.usage_colors(provider),
            metrics.provider_top(provider),
            layout,
            &mut cursor,
        );
    }

    for provider in Provider::ALL {
        let visible = state.is_provider_visible(provider);
        draw_chip(
            dc,
            layout,
            metrics.provider_toggle_rect(provider, layout),
            provider.name(),
            visible,
        );
    }
    let startup_label = if state.start_with_windows.load(Ordering::Relaxed) {
        "Start with Windows ✓"
    } else {
        "Start with Windows"
    };
    for (rect, label) in [
        (metrics.startup_rect(layout), startup_label),
        (metrics.sources_rect(layout), "Sources"),
        (metrics.exit_rect(layout), "Exit"),
    ] {
        draw_text(
            dc,
            layout,
            rect,
            label,
            Font::regular(11),
            layout.colors().muted_text,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }
    cursor.animating
}

fn paint_provider(
    dc: HDC,
    snapshot: ProviderSnapshot,
    service_status: ServiceStatus,
    colors: UsageColors,
    top: i32,
    layout: Layout,
    cursor: &mut PulseCursor,
) {
    let now = now_unix();
    let card = CardLayout::new(&snapshot);
    let card_bottom = top + card.height - CARD_GAP;
    fill_round_rect(
        dc,
        layout.rect(CONTENT_LEFT, top, CONTENT_RIGHT, card_bottom),
        layout.px(CARD_RADIUS),
        layout.colors().card,
    );

    let header_top = top + CARD_PAD_TOP;
    let header_rect = |left: i32, right: i32| {
        layout.rect(left, header_top, right, header_top + CARD_HEADER_HEIGHT - 8)
    };
    let name_width = measure_text(dc, layout, snapshot.provider.name(), Font::bold(14));
    draw_text(
        dc,
        layout,
        header_rect(CARD_LEFT, CARD_LEFT + name_width + 4),
        snapshot.provider.name(),
        Font::bold(14),
        layout.colors().text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let dot_left = CARD_LEFT + name_width + 8;
    let dot_top = header_top + (CARD_HEADER_HEIGHT - 8) / 2 - 3;
    fill_ellipse(
        dc,
        layout.rect(dot_left, dot_top, dot_left + 6, dot_top + 6),
        layout
            .theme
            .dashboard_usage_color(palette::service_status_color(service_status.level)),
    );
    draw_text(
        dc,
        layout,
        header_rect(dot_left + 11, 250),
        service_status.label(),
        Font::regular(11),
        layout.colors().muted_text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    if !snapshot.from_session_history {
        draw_text(
            dc,
            layout,
            header_rect(250, CARD_RIGHT),
            &if snapshot.error.is_some() {
                "Refresh failed".into()
            } else {
                snapshot.freshness_label(now)
            },
            Font::regular(11),
            layout.colors().dim_text,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
        );
    }

    for row in card.rows {
        let row_top = top + row.top;
        match row.content {
            CardRow::History => draw_text(
                dc,
                layout,
                layout.rect(CARD_LEFT, row_top, CARD_RIGHT, row_top + HISTORY_ROW_HEIGHT),
                &snapshot.freshness_label(now),
                Font::regular(11),
                layout.colors().dim_text,
                DT_LEFT | DT_SINGLELINE,
            ),
            CardRow::Window(window) => {
                paint_window_row(dc, colors, window, row_top, layout, cursor)
            }
            CardRow::Models(windows) => paint_model_pills(dc, colors, windows, row_top, layout),
            CardRow::ExtraUsage(budget) => paint_extra_usage(dc, budget, row_top, layout),
            CardRow::Resets => {
                let label = match snapshot.reset_credits.as_ref() {
                    Some(credits) if snapshot.error.is_none() => {
                        format!("Reset ×{}", credits.available_count)
                    }
                    _ => "Reset".into(),
                };
                draw_text(
                    dc,
                    layout,
                    reset_rect(layout, row_top),
                    &label,
                    Font::regular(11),
                    if resets::is_open() {
                        layout.colors().off_text
                    } else {
                        layout.colors().dim_text
                    },
                    DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
                );
            }
            CardRow::Message(error) => {
                let message = error
                    .map(error_summary)
                    .unwrap_or_else(|| "Waiting for usage...".into());
                draw_text(
                    dc,
                    layout,
                    layout.rect(
                        CARD_LEFT,
                        row_top + 4,
                        CARD_RIGHT,
                        row_top + MESSAGE_ROW_HEIGHT,
                    ),
                    &message,
                    Font::regular(12),
                    layout.colors().muted_text,
                    DT_LEFT | DT_WORDBREAK,
                );
            }
        }
    }
}

/// A usage window: label, the usage bar with the elapsed-time bar beneath it,
/// the percentage, and the time until reset.
fn paint_window_row(
    dc: HDC,
    colors: UsageColors,
    window: &UsageWindow,
    top: i32,
    layout: Layout,
    cursor: &mut PulseCursor,
) {
    let now = now_unix();
    let applicable = window.is_applicable(now);
    let percentage = if applicable { window.used_percent } else { 0.0 };
    let label = if applicable {
        window.label.as_str()
    } else {
        "Expired window"
    };
    let color = layout.theme.dashboard_usage_color(colors.at(percentage));
    draw_text(
        dc,
        layout,
        layout.rect(CARD_LEFT, top + 4, BARS_RIGHT, top + 20),
        label,
        Font::regular(12),
        layout.colors().text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let track = layout.rect(CARD_LEFT, top + 25, BARS_RIGHT, top + 30);
    let fill_end = paint_meter(dc, track, percentage, color, layout.colors().card);
    if let Some(elapsed) = window.elapsed_percent(now).filter(|_| applicable) {
        let elapsed_track = layout.rect(CARD_LEFT, top + 34, BARS_RIGHT, top + 36);
        paint_meter(
            dc,
            elapsed_track,
            elapsed,
            blend(layout.colors().card, layout.colors().elapsed, 0.5),
            layout.colors().card,
        );
    }
    draw_text(
        dc,
        layout,
        layout.rect(BARS_RIGHT + 12, top + 18, BARS_RIGHT + 56, top + 34),
        &format!("{:.0}%", percentage),
        Font::bold(13),
        layout.colors().text,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
    );
    let countdown = format_countdown(window.resets_at_unix, now);
    let countdown = match countdown.strip_prefix("resets in ") {
        Some(remaining) => remaining,
        None if window.resets_at_unix.is_none() => "no reset",
        None => countdown.as_str(),
    };
    draw_text(
        dc,
        layout,
        layout.rect(BARS_RIGHT + 68, top + 20, CARD_RIGHT, top + 34),
        countdown,
        Font::regular(11),
        layout.colors().dim_text,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
    );
    paint_trace(dc, track, fill_end, color, layout, cursor);
}

/// Fills a bar and returns the device-pixel x where the fill ends.
fn paint_meter(dc: HDC, track: RECT, percentage: f64, color: Rgb, surface: Rgb) -> i32 {
    fill_rect(dc, track, blend(surface, color, 0.2));
    let mut fill = track;
    fill.right = fill.left + ((fill.right - fill.left) as f64 * percentage / 100.0).round() as i32;
    if fill.right > fill.left {
        fill_rect(dc, fill, color);
    }
    fill.right
}

/// Draws this bar's part of the refresh trace and advances the pulse cursor.
fn paint_trace(
    dc: HDC,
    track: RECT,
    fill_end: i32,
    color: Rgb,
    layout: Layout,
    cursor: &mut PulseCursor,
) {
    let index = cursor.next_bar;
    cursor.next_bar += 1;
    let Some(clock) = cursor.clock else {
        return;
    };
    let scale = f64::from(layout.scale);
    let width = f64::from(track.right - track.left) / scale;
    let trace = Trace::build(
        width,
        f64::from(fill_end - track.left) / scale,
        clock.seed,
        index,
    );
    let local_ms = clock.elapsed_ms - index as f64 * pulse::STAGGER_MS;
    if local_ms < 0.0 {
        cursor.animating = true;
        return;
    }
    let Some(frame) = Frame::at(&trace, local_ms) else {
        return;
    };
    cursor.animating = true;
    let center_y = f64::from(track.top + track.bottom) / 2.0;
    let to_device = |points: &[(f64, f64)]| {
        points
            .iter()
            .map(|(x, y)| POINT {
                x: (f64::from(track.left) + x * scale).round() as i32,
                y: (center_y + y * scale).round() as i32,
            })
            .collect::<Vec<_>>()
    };
    draw_polyline(
        dc,
        &to_device(&frame.trail),
        blend(layout.colors().card, color, 0.55 * frame.trail_opacity),
        1,
    );
    draw_polyline(
        dc,
        &to_device(&frame.head),
        layout.colors().trace_head,
        ((1.5 * scale) as i32).max(1),
    );
}

fn paint_model_pills(
    dc: HDC,
    colors: UsageColors,
    windows: &[UsageWindow],
    top: i32,
    layout: Layout,
) {
    let now = now_unix();
    let mut left = CARD_LEFT;
    for window in windows {
        let applicable = window.is_applicable(now);
        let percentage = if applicable { window.used_percent } else { 0.0 };
        let text = format!("{} · {:.0}%", window.label, percentage);
        let width = measure_text(dc, layout, &text, Font::regular(11)) + 16;
        let right = (left + width).min(CARD_RIGHT);
        if right - left < 24 {
            break;
        }
        let pill = layout.rect(left, top + 2, right, top + 26);
        fill_round_rect(dc, pill, layout.px(6), layout.colors().pill);
        draw_text(
            dc,
            layout,
            layout.rect(left, top + 2, right, top + 24),
            &text,
            Font::regular(11),
            layout.colors().soft_text,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        let color = layout.theme.dashboard_usage_color(colors.at(percentage));
        paint_meter(
            dc,
            layout.rect(left + 4, top + 24, right - 4, top + 26),
            percentage,
            color,
            layout.colors().pill,
        );
        left = right + 6;
    }
}

fn paint_extra_usage(dc: HDC, budget: &ExtraUsageBudget, top: i32, layout: Layout) {
    fill_rect(
        dc,
        layout.rect(CARD_LEFT, top + 2, CARD_RIGHT, top + 3),
        layout.colors().card_rule,
    );
    let text_rect = |left: i32, right: i32| layout.rect(left, top + 9, right, top + 27);
    draw_text(
        dc,
        layout,
        text_rect(CARD_LEFT, 180),
        "Extra usage",
        Font::regular(11),
        layout.colors().dim_text,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let (amount, suffix) = match budget.remaining_minor().zip(budget.limit_minor) {
        Some((remaining, limit)) => (
            budget.format_amount(remaining),
            format!(" left of {}", budget.format_amount(limit)),
        ),
        None => (
            budget.format_amount(budget.used_minor),
            " spent · no cap".to_string(),
        ),
    };
    let suffix_width = measure_text(dc, layout, &suffix, Font::regular(11));
    draw_text(
        dc,
        layout,
        text_rect(180, CARD_RIGHT),
        &suffix,
        Font::regular(11),
        layout.colors().dim_text,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
    );
    draw_text(
        dc,
        layout,
        text_rect(180, CARD_RIGHT - suffix_width),
        &amount,
        Font::semibold(11),
        layout.colors().text,
        DT_RIGHT | DT_VCENTER | DT_SINGLELINE,
    );
}

fn draw_button(dc: HDC, rect: RECT, label: &str, layout: Layout) {
    fill_round_rect(dc, rect, layout.px(6), layout.colors().button);
    draw_text(
        dc,
        layout,
        rect,
        label,
        Font::regular(12),
        layout.colors().text,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

fn draw_chip(dc: HDC, layout: Layout, rect: RECT, label: &str, on: bool) {
    fill_round_rect(dc, rect, rect.bottom - rect.top, layout.colors().chip);
    let font = Font::regular(11);
    draw_text(
        dc,
        layout,
        rect,
        label,
        if on { font } else { font.strikeout() },
        if on {
            layout.colors().soft_text
        } else {
            layout.colors().off_text
        },
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

fn layout_for_window(hwnd: HWND) -> Layout {
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    Layout {
        scale: dpi as f32 / 96.0,
        theme: state_from_window(hwnd)
            .map(SharedState::tray_theme)
            .unwrap_or_default(),
    }
}

fn point_in_rect(x: i32, y: i32, rect: RECT) -> bool {
    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn copy_wide_fixed<const N: usize>(value: &str, target: &mut [u16; N]) {
    for (destination, source) in target.iter_mut().zip(value.encode_utf16().chain(Some(0))) {
        *destination = source;
    }
}

fn reset_rect(layout: Layout, row_top: i32) -> RECT {
    layout.rect(
        CARD_LEFT,
        row_top,
        CARD_LEFT + 100,
        row_top + RESET_ROW_HEIGHT,
    )
}

/// Opens a standalone visual preview without starting providers or redeeming credits.
pub fn preview_resets() -> Result<(), String> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }.ok();
    resets::preview()
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
        let card_chrome = CARD_PAD_TOP + CARD_HEADER_HEIGHT + CARD_PAD_BOTTOM + CARD_GAP;
        assert_eq!(
            CardLayout::new(&snapshot).height,
            card_chrome + MESSAGE_ROW_HEIGHT
        );
        let summary = error_summary(snapshot.error.as_ref().unwrap());
        assert!(summary.len() < 110);
        assert!(summary.ends_with("Click for details."));
        snapshot.session = Some(UsageWindow::new("Session", 10.0, None));
        assert_eq!(
            CardLayout::new(&snapshot).height,
            card_chrome + WINDOW_ROW_HEIGHT + MESSAGE_ROW_HEIGHT
        );
    }

    #[test]
    fn model_usage_windows_share_one_row_of_pills() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Claude);
        snapshot.session = Some(UsageWindow::new("Session", 10.0, None));
        snapshot.weekly = Some(UsageWindow::new("Weekly", 20.0, None));
        let without_model_window = CardLayout::new(&snapshot).height;

        snapshot
            .model_windows
            .push(UsageWindow::new("Fable 5", 30.0, None));
        assert_eq!(
            CardLayout::new(&snapshot).height,
            without_model_window + MODEL_ROW_HEIGHT
        );

        snapshot
            .model_windows
            .push(UsageWindow::new("Sonnet", 30.0, None));
        assert_eq!(
            CardLayout::new(&snapshot).height,
            without_model_window + MODEL_ROW_HEIGHT
        );
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
            TrayTheme::Dark,
        );

        assert_eq!(pixels[32 * 64 + 54], bgra(palette::BACKGROUND));
    }

    #[test]
    fn light_tray_uses_light_background_and_track_without_changing_ring_or_transparency() {
        let theme = TrayTheme::Light;
        let color = theme.usage_color(UsageColors::for_provider(Provider::Codex).normal);
        for status in [
            ServiceStatusLevel::Operational,
            ServiceStatusLevel::MajorOutage,
        ] {
            let mut pixels = vec![0; 64 * 64];
            render_circle_pixels(&mut pixels, 64, 50.0, color, status, theme);
            assert_eq!(pixels[32 * 64 + 32], bgra(theme.background()));
            assert_eq!(pixels[32 * 64 + 58], bgra(color));
            assert_eq!(pixels[32 * 64 + 5], bgra(theme.track()));
            assert_eq!(pixels[0], 0);
        }
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
            TrayTheme::Dark,
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
            TrayTheme::Dark,
        );
        render_circle_pixels(
            &mut major,
            64,
            50.0,
            Rgb(255, 255, 255),
            ServiceStatusLevel::MajorOutage,
            TrayTheme::Dark,
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
            TrayTheme::Dark,
        );

        assert_eq!(pixels[32 * 64 + 58], bgra(usage_color));
    }
}
