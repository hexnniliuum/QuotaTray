use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::drawing::{Font, Layout, colorref, create_font, draw_text, fill_rect, fill_round_rect};
use crate::palette::TrayTheme;
use windows::Win32::Foundation::{FILETIME, HWND, LPARAM, LRESULT, SYSTEMTIME, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::CoCreateGuid;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTimeEx};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::app::SharedState;
use crate::model::{Provider, ResetCredit, now_unix};
use crate::providers::codex::{ResetOutcome, ResetSession};
use crate::settings::Settings;

static OPEN: AtomicBool = AtomicBool::new(false);
const DETAILS: usize = 101;
const CLASS: PCWSTR = w!("QuotaTray.ResetConfirmation");

pub(super) fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

pub(super) fn open(state: Arc<SharedState>) {
    if OPEN.swap(true, Ordering::Relaxed) {
        return;
    }
    state.clear_notice();
    let worker_state = state.clone();
    if std::thread::Builder::new()
        .name("codex-reset-confirmation".into())
        .spawn(move || {
            let result = run(&worker_state);
            OPEN.store(false, Ordering::Relaxed);
            match result {
                Ok(()) => worker_state.notify_ui(),
                Err(error) => {
                    worker_state.report_error(&error);
                    // The dialog hid the dashboard, so the notice alone would go unseen.
                    let _ = confirm(&error, None, false, worker_state.tray_theme(), false);
                }
            }
        })
        .is_err()
    {
        OPEN.store(false, Ordering::Relaxed);
        state.report_error("Could not open the reset confirmation.");
    }
}

fn run(state: &SharedState) -> Result<(), String> {
    let config = Settings::load()?.providers[Provider::Codex.index()].clone();
    let mut session = ResetSession::open(&config)?;
    let credits = session.snapshot.reset_credits.as_ref()
        .ok_or("This Codex version or account did not provide reset availability. Update Codex and try again.")?;
    if credits.available_count == 0 {
        confirm(
            "No resets are available for this Codex account.",
            None,
            false,
            state.tray_theme(),
            false,
        )?;
        state.request_usage_refresh();
        return Ok(());
    }
    let choices = credits.choices(now_unix());
    let first = choices.first().cloned();
    let auto = credits.credits.is_none() || choices.len() < credits.available_count as usize;
    let selection = first.as_ref().map(|credit| credit.id.clone());
    let detail = match &first {
        Some(credit) => format!(
            "{} · {}",
            credit.title,
            expiry_label(credit.expires_at, credit.expiry_known, now_unix())
        ),
        None if auto => "Codex will use the next available reset.".into(),
        None => "No unexpired resets are available.".into(),
    };
    let content = format!(
        "{} · {}\r\n{detail}",
        session.account_label(),
        session.source.label()
    );
    if !confirm(&content, first, auto, state.tray_theme(), false)? {
        return Ok(());
    }
    if Settings::load()?.providers[Provider::Codex.index()] != config {
        return Err("Source settings changed. Reopen Reset.".into());
    }
    let key = unsafe { CoCreateGuid() }.map_err(|_| "Could not prepare a reset request.")?;
    let result = session.consume(selection.as_deref(), &format!("{key:?}"));
    state.request_usage_refresh();
    match result? {
        ResetOutcome::Applied | ResetOutcome::AlreadyApplied => {}
        ResetOutcome::NothingToReset => {
            return Err("There is no eligible usage window to reset. Nothing was applied.".into());
        }
        ResetOutcome::NoCredit => {
            return Err("No resets are available. Nothing was applied.".into());
        }
    }
    Ok(())
}

pub(super) fn preview() -> Result<(), String> {
    let theme = Settings::load()?.appearance?.tray_theme;
    let credit = ResetCredit {
        id: "preview-only".into(),
        title: "Sample reset".into(),
        expires_at: Some(now_unix() + 2 * 86400),
        expiry_known: true,
    };
    confirm(
        "Preview · sample reset\r\nPreview only · Applying disabled.",
        Some(credit),
        false,
        theme,
        true,
    )?;
    Ok(())
}

struct DialogStyle {
    layout: Layout,
    background: HBRUSH,
    font: HFONT,
    icon: HICON,
}

impl DialogStyle {
    fn new(theme: TrayTheme) -> Self {
        let layout = Layout {
            scale: unsafe { GetDpiForSystem() } as f32 / 96.0,
            theme,
        };
        Self {
            layout,
            background: unsafe { CreateSolidBrush(colorref(layout.colors().background)) },
            font: create_font(layout, Font::regular(11)),
            icon: super::create_circle_icon(
                "Q",
                100.0,
                layout.colors().text,
                crate::model::ServiceStatusLevel::Operational,
                theme,
            ),
        }
    }
}

impl Drop for DialogStyle {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.background.into());
            let _ = DeleteObject(self.font.into());
            let _ = DestroyIcon(self.icon);
        }
    }
}

struct Confirmation {
    credit: Option<ResetCredit>,
    auto: bool,
    confirmed: bool,
    style: DialogStyle,
    preview: bool,
}

impl Confirmation {
    fn can_apply(&self) -> bool {
        !self.preview && (self.credit.is_some() || self.auto)
    }
}

fn confirm(
    content: &str,
    credit: Option<ResetCredit>,
    auto: bool,
    theme: TrayTheme,
    preview: bool,
) -> Result<bool, String> {
    let mut data = Confirmation {
        credit,
        auto,
        confirmed: false,
        style: DialogStyle::new(theme),
        preview,
    };
    unsafe {
        let hwnd = create_confirmation(content, &mut data)?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0).0;
            if result == -1 {
                DestroyWindow(hwnd).ok();
                return Err("The reset dialog stopped unexpectedly.".into());
            }
            if result == 0 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(data.confirmed)
}

// The caller keeps data at a stable address until the returned window is destroyed.
unsafe fn create_confirmation(content: &str, data: &mut Confirmation) -> Result<HWND, String> {
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|_| "Could not load the dialog.")?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(dialog_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let layout = data.style.layout;
        let px = |v| layout.px(v);
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            if data.preview {
                w!("QuotaTray (preview)")
            } else {
                w!("QuotaTray")
            },
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            px(360),
            px(170),
            None,
            None,
            Some(instance.into()),
            Some((data as *mut Confirmation).cast()),
        )
        .map_err(|_| "Could not create the reset dialog.")?;
        for kind in [ICON_SMALL, ICON_BIG] {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(WPARAM(kind as usize)),
                Some(LPARAM(data.style.icon.0 as isize)),
            );
        }
        let dark: i32 = i32::from(layout.theme == TrayTheme::Dark);
        let caption = colorref(layout.colors().background);
        let text = colorref(layout.colors().text);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const i32).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CAPTION_COLOR,
            std::ptr::from_ref(&caption).cast(),
            4,
        );
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TEXT_COLOR, std::ptr::from_ref(&text).cast(), 4);
        let result: Result<(), String> = (|| {
            let control = |class: PCWSTR,
                           text: &str,
                           style: WINDOW_STYLE,
                           id: usize,
                           rect: [i32; 4]|
             -> Result<HWND, String> {
                let text = crate::wide(text);
                let control = CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    class,
                    PCWSTR(text.as_ptr()),
                    WS_CHILD | WS_VISIBLE | style,
                    px(rect[0]),
                    px(rect[1]),
                    px(rect[2]),
                    px(rect[3]),
                    Some(hwnd),
                    Some(HMENU(id as *mut _)),
                    Some(instance.into()),
                    None,
                )
                .map_err(|_| "Could not create a reset dialog control.".to_string())?;
                SendMessageW(
                    control,
                    WM_SETFONT,
                    Some(WPARAM(data.style.font.0 as usize)),
                    Some(LPARAM(1)),
                );
                Ok(control)
            };
            control(
                w!("STATIC"),
                if data.credit.is_some() || data.auto {
                    "Apply reset?"
                } else {
                    "Codex reset"
                },
                WINDOW_STYLE::default(),
                0,
                [14, 12, 316, 18],
            )?;
            control(
                w!("STATIC"),
                content,
                WINDOW_STYLE::default(),
                DETAILS,
                [14, 36, 316, 48],
            )?;
            control(
                w!("BUTTON"),
                "Yes",
                WS_TABSTOP
                    | WINDOW_STYLE(BS_OWNERDRAW as u32)
                    | if data.can_apply() {
                        WINDOW_STYLE::default()
                    } else {
                        WS_DISABLED
                    },
                IDYES.0 as usize,
                [196, 96, 62, 26],
            )?;
            control(
                w!("BUTTON"),
                "No",
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                IDNO.0 as usize,
                [268, 96, 62, 26],
            )?;
            Ok(())
        })();
        if result.is_err() {
            DestroyWindow(hwnd).ok();
        }
        result?;
        Ok(hwnd)
    }
}

unsafe extern "system" fn dialog_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCCREATE {
            let create = &*(lp.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        let data = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Confirmation).as_mut();
        match msg {
            DM_GETDEFID => LRESULT(IDNO.0 as isize | ((DC_HASDEFID as isize) << 16)),
            WM_ERASEBKGND => {
                if let Some(data) = data {
                    let mut rect = Default::default();
                    let _ = GetClientRect(hwnd, &mut rect);
                    fill_rect(
                        HDC(wp.0 as *mut _),
                        rect,
                        data.style.layout.colors().background,
                    );
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_CTLCOLORSTATIC => {
                if let Some(data) = data {
                    let dc = HDC(wp.0 as *mut _);
                    let colors = data.style.layout.colors();
                    SetTextColor(dc, colorref(colors.muted_text));
                    SetBkColor(dc, colorref(colors.background));
                    return LRESULT(data.style.background.0 as isize);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_DRAWITEM => {
                if let Some(data) = data {
                    paint_item(&*(lp.0 as *const DRAWITEMSTRUCT), data);
                    return LRESULT(1);
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = wp.0 & 0xffff;
                if id == IDNO.0 as usize || id == IDCANCEL.0 as usize {
                    DestroyWindow(hwnd).ok();
                } else if id == IDYES.0 as usize
                    && let Some(data) = data
                {
                    if !data.can_apply() {
                        return LRESULT(0);
                    }
                    if data.credit.as_ref().is_some_and(|credit| {
                        credit.expires_at.is_some_and(|expiry| expiry <= now_unix())
                    }) {
                        if let Ok(details) = GetDlgItem(Some(hwnd), DETAILS as i32) {
                            let _ = SetWindowTextW(
                                details,
                                w!("This reset has expired. Reopen Reset to try again."),
                            );
                        }
                        return LRESULT(0);
                    }
                    data.confirmed = true;
                    DestroyWindow(hwnd).ok();
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn paint_item(item: &DRAWITEMSTRUCT, data: &Confirmation) {
    let layout = data.style.layout;
    let colors = layout.colors();
    let dc = item.hDC;
    let selected = item.itemState.0 & ODS_SELECTED.0 != 0;
    let focused = item.itemState.0 & ODS_FOCUS.0 != 0;
    unsafe {
        SetBkMode(dc, TRANSPARENT);
    }
    fill_rect(dc, item.rcItem, colors.background);
    let mut rect = item.rcItem;
    rect.bottom -= layout.px(4);
    fill_round_rect(
        dc,
        rect,
        layout.px(7),
        if selected { colors.pill } else { colors.button },
    );
    draw_text(
        dc,
        layout,
        rect,
        if item.CtlID == IDYES.0 as u32 {
            "Yes"
        } else {
            "No"
        },
        Font::semibold(11),
        if item.itemState.0 & ODS_DISABLED.0 != 0 {
            colors.off_text
        } else {
            colors.text
        },
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    if focused {
        rect.left += layout.px(3);
        rect.right -= layout.px(3);
        rect.top += layout.px(3);
        rect.bottom -= layout.px(3);
        unsafe {
            let _ = DrawFocusRect(dc, &rect);
        }
    }
}

fn expiry_label(expiry: Option<i64>, known: bool, now: i64) -> String {
    let Some(expiry) = expiry else {
        return if known {
            "Does not expire"
        } else {
            "Expiry unavailable"
        }
        .into();
    };
    let remaining = expiry.saturating_sub(now);
    let relative = if remaining <= 0 {
        "Expired".into()
    } else if remaining >= 86400 {
        format!(
            "Expires in {}d {}h",
            remaining / 86400,
            remaining % 86400 / 3600
        )
    } else if remaining >= 3600 {
        format!(
            "Expires in {}h {}m",
            remaining / 3600,
            remaining % 3600 / 60
        )
    } else {
        format!("Expires in {}m", (remaining / 60).max(1))
    };
    match local_date(expiry) {
        Some(date) => format!("{relative} · {date} (local)"),
        None => relative,
    }
}

fn local_date(unix: i64) -> Option<String> {
    let ticks = u64::try_from(unix.checked_add(11_644_473_600)?)
        .ok()?
        .checked_mul(10_000_000)?;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        FileTimeToSystemTime(&file, &mut utc).ok()?;
        SystemTimeToTzSpecificLocalTimeEx(None, &utc, &mut local).ok()?;
    }
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_confirmation_requires_yes_and_blocks_unavailable_resets() {
        std::thread::spawn(|| unsafe {
            for (credit, auto, preview, command, expected) in [
                (true, false, false, IDYES, true),
                (true, false, false, IDNO, false),
                (true, false, false, IDCANCEL, false),
                (false, true, false, IDYES, true),
                (false, false, false, IDYES, false),
                (true, false, true, IDYES, false),
            ] {
                let mut data = Confirmation {
                    credit: credit.then(|| ResetCredit {
                        id: "first".into(),
                        title: "First reset".into(),
                        expires_at: Some(now_unix() + 3600),
                        expiry_known: true,
                    }),
                    auto,
                    preview,
                    confirmed: false,
                    style: DialogStyle::new(if preview {
                        TrayTheme::Light
                    } else {
                        TrayTheme::Dark
                    }),
                };
                let hwnd = create_confirmation("Test account · Windows", &mut data).unwrap();
                let mut title = [0u16; 128];
                let length = GetWindowTextW(hwnd, &mut title) as usize;
                assert_eq!(
                    String::from_utf16_lossy(&title[..length]),
                    if preview {
                        "QuotaTray (preview)"
                    } else {
                        "QuotaTray"
                    }
                );
                for kind in [ICON_SMALL, ICON_BIG] {
                    assert_eq!(
                        SendMessageW(hwnd, WM_GETICON, Some(WPARAM(kind as usize)), None).0,
                        data.style.icon.0 as isize
                    );
                    assert!(!data.style.icon.0.is_null());
                }
                let yes = GetDlgItem(Some(hwnd), IDYES.0).unwrap();
                assert_eq!(
                    GetWindowLongW(yes, GWL_STYLE) as u32 & WS_DISABLED.0 != 0,
                    !data.can_apply()
                );
                assert_eq!(
                    SendMessageW(hwnd, DM_GETDEFID, None, None).0 & 0xffff,
                    IDNO.0 as isize
                );
                SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(command.0 as usize)), None);
                assert_eq!(data.confirmed, expected);
                if command == IDYES && !data.can_apply() {
                    assert!(IsWindow(Some(hwnd)).as_bool());
                    SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDNO.0 as usize)), None);
                }
                assert!(!IsWindow(Some(hwnd)).as_bool());
            }

            let mut data = Confirmation {
                credit: Some(ResetCredit {
                    id: "expired".into(),
                    title: "Expired reset".into(),
                    expires_at: Some(now_unix() - 1),
                    expiry_known: true,
                }),
                auto: false,
                preview: false,
                confirmed: false,
                style: DialogStyle::new(TrayTheme::Dark),
            };
            let hwnd = create_confirmation("Test account · Windows", &mut data).unwrap();
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDYES.0 as usize)), None);
            assert!(IsWindow(Some(hwnd)).as_bool());
            assert!(!data.confirmed);
            SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDNO.0 as usize)), None);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn expiry_distinguishes_unknown_expired_and_remaining() {
        assert_eq!(expiry_label(None, false, 100), "Expiry unavailable");
        assert_eq!(expiry_label(None, true, 100), "Does not expire");
        assert!(expiry_label(Some(100), true, 100).starts_with("Expired"));
        assert!(expiry_label(Some(101), true, 100).starts_with("Expires in 1m"));
        assert!(expiry_label(Some(90100), true, 100).starts_with("Expires in 1d 1h"));
        assert!(expiry_label(Some(90100), true, 100).ends_with("(local)"));
        assert!(local_date(i64::MAX).is_none());
    }
}
