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
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_FOCUS, ODS_SELECTED,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::app::{SharedState, WM_USAGE_UPDATED};
use crate::model::{Provider, ResetCredit, format_countdown, now_unix};
use crate::providers::codex::{ResetOutcome, ResetSession};
use crate::settings::Settings;

static OPEN: AtomicBool = AtomicBool::new(false);
const LIST: usize = 100;
const DETAILS: usize = 101;
const CLASS: PCWSTR = w!("QuotaTray.ResetChooser");

pub(super) fn is_open() -> bool {
    OPEN.load(Ordering::Relaxed)
}

pub(super) fn open(state: Arc<SharedState>) {
    if OPEN.swap(true, Ordering::Relaxed) {
        return;
    }
    state.clear_notice();
    notify(&state);
    let worker_state = state.clone();
    if std::thread::Builder::new()
        .name("codex-reset-chooser".into())
        .spawn(move || {
            let result = run(&worker_state);
            if let Err(error) = result {
                worker_state.report_error(&error);
            }
            OPEN.store(false, Ordering::Relaxed);
            notify(&worker_state);
        })
        .is_err()
    {
        OPEN.store(false, Ordering::Relaxed);
        state.report_error("Could not open the reset chooser.");
        notify(&state);
    }
}

fn notify(state: &SharedState) {
    if let Some(hwnd) = state.tray_window() {
        unsafe { PostMessageW(Some(hwnd), WM_USAGE_UPDATED, WPARAM(0), LPARAM(0)) }.ok();
    }
}

fn run(state: &SharedState) -> Result<(), String> {
    let config = Settings::load()?.providers[Provider::Codex.index()].clone();
    let mut session = ResetSession::open(&config)?;
    let credits = session.snapshot.reset_credits.as_ref()
        .ok_or("This Codex version or account did not provide reset availability. Update Codex and try again.")?;
    if credits.available_count == 0 {
        choose(
            "No resets are available for this Codex account.",
            Vec::new(),
            false,
            state.tray_theme(),
            false,
        )?;
        return Ok(());
    }
    let choices = credits.choices(now_unix());
    let mut content = format!(
        "{} · {}\r\n{} resets available. Applying consumes one reset.\r\n",
        session.account_label(),
        session.source.label(),
        credits.available_count
    );
    for window in [
        session.snapshot.session.as_ref(),
        session.snapshot.weekly.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        content.push_str(&format!(
            "{}: {:.0}% used · {}\r\n",
            window.label,
            window.used_percent,
            format_countdown(window.resets_at_unix, now_unix())
        ));
    }
    if choices.len() < credits.available_count as usize {
        content.push_str(
            "Some reset details are unavailable. Codex can choose the next available reset.\r\n",
        );
    }
    let offer_auto = credits.credits.is_none() || choices.len() < credits.available_count as usize;
    let selection = choose(&content, choices, offer_auto, state.tray_theme(), false)?;
    let Some(selection) = selection else {
        return Ok(());
    };
    if Settings::load()?.providers[Provider::Codex.index()] != config {
        return Err("Source settings changed. Reopen Available resets.".into());
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
    let choices = [("Sample reset", 2 * 86400), ("Sample reset", 7 * 86400)]
        .map(|(title, remaining)| ResetCredit {
            id: "preview-only".into(),
            title: title.into(),
            description:
                "Example reset for previewing the chooser. No account changes can be made here."
                    .into(),
            expires_at: Some(now_unix() + remaining),
            expiry_known: true,
        })
        .to_vec();
    choose(
        "Preview · sample resets\r\nThese are examples, not resets on your account.\r\nApplying is disabled in this preview.",
        choices,
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
            font: create_font(layout, Font::regular(14)),
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

struct Chooser {
    choices: Vec<ResetCredit>,
    auto: bool,
    selected: Option<Option<String>>,
    style: DialogStyle,
    preview: bool,
}

fn choose(
    content: &str,
    choices: Vec<ResetCredit>,
    auto: bool,
    theme: TrayTheme,
    preview: bool,
) -> Result<Option<Option<String>>, String> {
    let mut data = Chooser {
        choices,
        auto,
        selected: None,
        style: DialogStyle::new(theme),
        preview,
    };
    unsafe {
        let hwnd = create_chooser(content, &mut data)?;
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
        if IsWindow(Some(hwnd)).as_bool() {
            DestroyWindow(hwnd).ok();
        }
    }
    Ok(data.selected)
}

// The caller keeps data at a stable address until the returned window is destroyed.
unsafe fn create_chooser(content: &str, data: &mut Chooser) -> Result<HWND, String> {
    unsafe {
        let instance = GetModuleHandleW(None).map_err(|_| "Could not load the dialog.")?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(dialog_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),

            ..Default::default()
        };
        // The class remains registered for subsequent chooser threads.
        RegisterClassW(&class);
        let layout = data.style.layout;
        let px = |v| layout.px(v);
        let rows = (data.choices.len() + usize::from(data.auto)).clamp(1, 4) as i32;
        let list_height = rows * 60;
        let details_top = 158 + list_height;
        let buttons_top = details_top + 72;
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            CLASS,
            if data.preview {
                w!("QuotaTray — Codex resets (preview)")
            } else {
                w!("QuotaTray — Codex resets")
            },
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            px(600),
            px(buttons_top + 90),
            None,
            None,
            Some(instance.into()),
            Some((data as *mut Chooser).cast()),
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
                content,
                WINDOW_STYLE::default(),
                0,
                [20, 16, 544, 106],
            )?;
            control(
                w!("STATIC"),
                "Choose a reset (soonest expiry first):",
                WINDOW_STYLE::default(),
                0,
                [20, 126, 544, 22],
            )?;
            let list = control(
                w!("LISTBOX"),
                "",
                WS_TABSTOP
                    | WS_VSCROLL
                    | WINDOW_STYLE((LBS_NOTIFY | LBS_OWNERDRAWFIXED | LBS_HASSTRINGS) as u32),
                LIST,
                [20, 152, 544, list_height],
            )?;
            for credit in &data.choices {
                let text = crate::wide(format!(
                    "{} — {}",
                    credit.title,
                    expiry_label(credit.expires_at, credit.expiry_known, now_unix())
                ));
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    None,
                    Some(LPARAM(text.as_ptr() as isize)),
                );
            }
            if data.choices.is_empty() && !data.auto {
                let text = crate::wide("No resets available");
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    None,
                    Some(LPARAM(text.as_ptr() as isize)),
                );
            }
            if data.auto {
                let text =
                    crate::wide("Let Codex choose the next available reset (expiry unavailable)");
                SendMessageW(
                    list,
                    LB_ADDSTRING,
                    None,
                    Some(LPARAM(text.as_ptr() as isize)),
                );
            }
            control(
                w!("STATIC"),
                "",
                WINDOW_STYLE::default(),
                DETAILS,
                [20, details_top, 544, 60],
            )?;
            control(
                w!("BUTTON"),
                "Apply selected reset",
                WS_TABSTOP
                    | WINDOW_STYLE(BS_OWNERDRAW as u32)
                    | if data.preview || (data.choices.is_empty() && !data.auto) {
                        WS_DISABLED
                    } else {
                        WINDOW_STYLE::default()
                    },
                IDOK.0 as usize,
                [274, buttons_top, 180, 34],
            )?;
            control(
                w!("BUTTON"),
                if data.preview || (data.choices.is_empty() && !data.auto) {
                    "Close"
                } else {
                    "Cancel"
                },
                WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
                IDCANCEL.0 as usize,
                [466, buttons_top, 98, 34],
            )?;
            SendMessageW(list, LB_SETCURSEL, Some(WPARAM(0)), None);
            update_details(hwnd, data);
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
        let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Chooser;
        let data = pointer.as_ref();
        match msg {
            DM_GETDEFID => LRESULT(IDOK.0 as isize | ((DC_HASDEFID as isize) << 16)),
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
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                if let Some(data) = data {
                    let dc = HDC(wp.0 as *mut _);
                    let colors = data.style.layout.colors();
                    SetTextColor(dc, colorref(colors.muted_text));
                    SetBkColor(dc, colorref(colors.background));
                    return LRESULT(data.style.background.0 as isize);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_MEASUREITEM => {
                if let Some(data) = data {
                    let item = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
                    item.itemHeight = data.style.layout.px(60) as u32;
                    return LRESULT(1);
                }
                LRESULT(0)
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
                if id == IDCANCEL.0 as usize {
                    DestroyWindow(hwnd).ok();
                } else if let Some(data) = data {
                    if id == LIST {
                        update_details(hwnd, data);
                    } else if id == IDOK.0 as usize {
                        if data.preview || (data.choices.is_empty() && !data.auto) {
                            return LRESULT(0);
                        }
                        let index = selected_index(hwnd);
                        if let Some(credit) = data.choices.get(index) {
                            if credit.expires_at.is_some_and(|expiry| expiry <= now_unix()) {
                                if let Ok(details) = GetDlgItem(Some(hwnd), DETAILS as i32) {
                                    let _ = SetWindowTextW(
                                        details,
                                        w!("This reset has expired. Choose another reset."),
                                    );
                                }
                                return LRESULT(0);
                            }
                            (*pointer).selected = Some(Some(credit.id.clone()));
                        } else if data.auto && index == data.choices.len() {
                            (*pointer).selected = Some(None);
                        } else {
                            return LRESULT(0);
                        }
                        DestroyWindow(hwnd).ok();
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                DestroyWindow(hwnd).ok();
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

fn paint_item(item: &DRAWITEMSTRUCT, data: &Chooser) {
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
    if item.CtlID as usize == LIST {
        if item.itemID == u32::MAX {
            return;
        }
        fill_round_rect(
            dc,
            rect,
            layout.px(8),
            if selected {
                colors.muted_text
            } else {
                colors.card
            },
        );
        rect.left += layout.px(1);
        rect.top += layout.px(1);
        rect.right -= layout.px(1);
        rect.bottom -= layout.px(1);
        fill_round_rect(
            dc,
            rect,
            layout.px(7),
            if selected { colors.button } else { colors.card },
        );
        let (title, expiry) = match data.choices.get(item.itemID as usize) {
            Some(credit) => (
                credit.title.as_str(),
                expiry_label(credit.expires_at, credit.expiry_known, now_unix()),
            ),
            None if !data.auto => (
                "No resets available",
                "New resets will appear here when available.".into(),
            ),
            None => (
                "Let Codex choose",
                "Next available reset · expiry unavailable".into(),
            ),
        };
        rect.left += layout.px(14);
        rect.right -= layout.px(10);
        let mut title_rect = rect;
        title_rect.top += layout.px(7);
        title_rect.bottom = title_rect.top + layout.px(20);
        draw_text(
            dc,
            layout,
            title_rect,
            title,
            Font::semibold(14),
            colors.text,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        let mut expiry_rect = rect;
        expiry_rect.top += layout.px(29);
        draw_text(
            dc,
            layout,
            expiry_rect,
            &expiry,
            Font::regular(12),
            colors.muted_text,
            DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
    } else {
        fill_round_rect(
            dc,
            rect,
            layout.px(7),
            if selected { colors.pill } else { colors.button },
        );
        let label = if item.CtlID == IDOK.0 as u32 {
            "Apply selected reset"
        } else if data.preview || (data.choices.is_empty() && !data.auto) {
            "Close"
        } else {
            "Cancel"
        };
        draw_text(
            dc,
            layout,
            rect,
            label,
            Font::semibold(13),
            if item.itemState.0 & ODS_DISABLED.0 != 0 {
                colors.off_text
            } else {
                colors.text
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
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

unsafe fn selected_index(hwnd: HWND) -> usize {
    unsafe {
        GetDlgItem(Some(hwnd), LIST as i32)
            .map(|list| SendMessageW(list, LB_GETCURSEL, None, None).0 as usize)
            .unwrap_or(usize::MAX)
    }
}

unsafe fn update_details(hwnd: HWND, data: &Chooser) {
    unsafe {
        let text = match data.choices.get(selected_index(hwnd)) {
            Some(credit) => credit.description.clone(),
            None if !data.auto => "Nothing to apply. You can close this window.".into(),
            None => "Codex chooses which reset to consume. Individual details and expiry are unavailable.".into(),
        };
        if let Ok(details) = GetDlgItem(Some(hwnd), DETAILS as i32) {
            let text = crate::wide(text);
            SetWindowTextW(details, PCWSTR(text.as_ptr())).ok();
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
    fn native_chooser_selects_exact_credit_auto_or_cancel() {
        std::thread::spawn(|| unsafe {
            for (index, command, expected) in [
                (0, IDOK, Some(Some("first".to_string()))),
                (1, IDOK, Some(Some("second".to_string()))),
                (2, IDOK, Some(None)),
                (0, IDCANCEL, None),
            ] {
                let mut data = Chooser {
                    choices: ["first", "second"]
                        .map(|id| ResetCredit {
                            id: id.into(),
                            title: id.into(),
                            description: "Test reset".into(),
                            expires_at: Some(now_unix() + 3600),
                            expiry_known: true,
                        })
                        .to_vec(),
                    auto: true,
                    preview: command == IDCANCEL,
                    selected: None,
                    style: DialogStyle::new(if index % 2 == 0 {
                        TrayTheme::Dark
                    } else {
                        TrayTheme::Light
                    }),
                };
                let hwnd = create_chooser("Test account · Windows", &mut data).unwrap();
                let mut title = [0u16; 128];
                let length = GetWindowTextW(hwnd, &mut title) as usize;
                assert_eq!(
                    String::from_utf16_lossy(&title[..length]),
                    if data.preview {
                        "QuotaTray — Codex resets (preview)"
                    } else {
                        "QuotaTray — Codex resets"
                    }
                );
                for kind in [ICON_SMALL, ICON_BIG] {
                    assert_eq!(
                        SendMessageW(hwnd, WM_GETICON, Some(WPARAM(kind as usize)), None).0,
                        data.style.icon.0 as isize
                    );
                    assert!(!data.style.icon.0.is_null());
                }
                let list = GetDlgItem(Some(hwnd), LIST as i32).unwrap();
                assert_eq!(SendMessageW(list, LB_GETCOUNT, None, None).0, 3);
                assert_eq!(selected_index(hwnd), 0);
                SendMessageW(list, LB_SETCURSEL, Some(WPARAM(index)), None);
                if data.preview {
                    let apply = GetDlgItem(Some(hwnd), IDOK.0).unwrap();
                    assert_ne!(GetWindowLongW(apply, GWL_STYLE) as u32 & WS_DISABLED.0, 0);
                    SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(IDOK.0 as usize)), None);
                    assert!(IsWindow(Some(hwnd)).as_bool());
                    assert_eq!(data.selected, None);
                }
                SendMessageW(hwnd, WM_COMMAND, Some(WPARAM(command.0 as usize)), None);
                assert!(!IsWindow(Some(hwnd)).as_bool());
                assert_eq!(data.selected, expected);
            }
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
