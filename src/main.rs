#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod diagnostics;
mod model;
mod palette;
mod process;
mod providers;
mod pulse;
mod registry;
mod service_status;
mod settings;
mod startup;
mod ui;
mod visibility;
mod winhttp;
mod wsl;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

fn main() {
    if std::env::args().any(|arg| arg == "--preview-resets") {
        if let Err(error) = ui::preview_resets() {
            report_startup_error(&error);
        }
        return;
    }
    let mutex = unsafe { CreateMutexW(None, false, w!("Local\\QuotaTray.SingleInstance")) };
    let Ok(mutex) = mutex else {
        return;
    };
    if windows::core::Error::from_win32().code() == ERROR_ALREADY_EXISTS.to_hresult() {
        unsafe { CloseHandle(mutex).ok() };
        return;
    }

    if let Err(error) = diagnostics::init() {
        report_startup_error(&format!(
            "Quota Tray could not initialize its diagnostic log.\n\n{error}"
        ));
    }

    let (state, receiver) = app::SharedState::new();
    app::start_refresh_worker(state.clone(), receiver);
    if let Err(error) = ui::run(state) {
        diagnostics::event("ERROR", &format!("UI loop stopped with an error: {error}"));
        report_startup_error(&format!("Quota Tray could not start.\n\n{error}"));
    }
    diagnostics::shutdown();
    unsafe { CloseHandle(mutex).ok() };
}

fn report_startup_error(message: &str) {
    diagnostics::event("ERROR", message);
    eprintln!("{message}");
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}
