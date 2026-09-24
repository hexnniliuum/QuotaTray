use std::ffi::c_void;
use std::ptr::{null, null_mut};

use crate::wide;

type HInternet = *mut c_void;

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const WINHTTP_ADDREQ_FLAG_ADD: u32 = 0x2000_0000;
const WINHTTP_ADDREQ_FLAG_REPLACE: u32 = 0x8000_0000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;

#[link(name = "winhttp")]
unsafe extern "system" {
    fn WinHttpOpen(
        user_agent: *const u16,
        access_type: u32,
        proxy_name: *const u16,
        proxy_bypass: *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpConnect(
        session: HInternet,
        server_name: *const u16,
        server_port: u16,
        reserved: u32,
    ) -> HInternet;
    fn WinHttpOpenRequest(
        connect: HInternet,
        verb: *const u16,
        object_name: *const u16,
        version: *const u16,
        referer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpSetTimeouts(
        internet: HInternet,
        resolve_timeout: i32,
        connect_timeout: i32,
        send_timeout: i32,
        receive_timeout: i32,
    ) -> i32;
    fn WinHttpAddRequestHeaders(
        request: HInternet,
        headers: *const u16,
        headers_length: u32,
        modifiers: u32,
    ) -> i32;
    fn WinHttpSendRequest(
        request: HInternet,
        headers: *const u16,
        headers_length: u32,
        optional: *mut c_void,
        optional_length: u32,
        total_length: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: HInternet, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: HInternet,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        buffer_length: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpQueryDataAvailable(request: HInternet, available: *mut u32) -> i32;
    fn WinHttpReadData(
        request: HInternet,
        buffer: *mut c_void,
        bytes_to_read: u32,
        bytes_read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(internet: HInternet) -> i32;
}

struct InternetHandle(HInternet);

impl InternetHandle {
    fn checked(handle: HInternet, operation: &str) -> Result<Self, String> {
        if handle.is_null() {
            Err(last_error(operation))
        } else {
            Ok(Self(handle))
        }
    }
}

impl Drop for InternetHandle {
    fn drop(&mut self) {
        unsafe {
            WinHttpCloseHandle(self.0);
        }
    }
}

pub fn get(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<(u32, Vec<u8>), String> {
    let agent = wide(concat!("QuotaTray/", env!("CARGO_PKG_VERSION")));
    let host = wide(host);
    let path = wide(path);
    let method = wide("GET");

    let session = InternetHandle::checked(
        unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            )
        },
        "opening HTTP session",
    )?;
    checked_bool(
        unsafe { WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 10_000) },
        "setting HTTP timeouts",
    )?;

    let connect = InternetHandle::checked(
        unsafe { WinHttpConnect(session.0, host.as_ptr(), 443, 0) },
        "connecting to usage service",
    )?;
    let request = InternetHandle::checked(
        unsafe {
            WinHttpOpenRequest(
                connect.0,
                method.as_ptr(),
                path.as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "opening usage request",
    )?;

    if !headers.is_empty() {
        let joined = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        let joined = wide(&joined);
        checked_bool(
            unsafe {
                WinHttpAddRequestHeaders(
                    request.0,
                    joined.as_ptr(),
                    u32::MAX,
                    WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
                )
            },
            "adding usage request headers",
        )?;
    }

    checked_bool(
        unsafe { WinHttpSendRequest(request.0, null(), 0, null_mut(), 0, 0, 0) },
        "sending usage request",
    )?;
    checked_bool(
        unsafe { WinHttpReceiveResponse(request.0, null_mut()) },
        "receiving usage response",
    )?;

    let mut status = 0u32;
    let mut status_size = std::mem::size_of::<u32>() as u32;
    checked_bool(
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                (&mut status as *mut u32).cast(),
                &mut status_size,
                null_mut(),
            )
        },
        "reading usage response status",
    )?;

    let mut body = Vec::new();
    loop {
        let mut available = 0u32;
        checked_bool(
            unsafe { WinHttpQueryDataAvailable(request.0, &mut available) },
            "reading usage response size",
        )?;
        if available == 0 {
            break;
        }
        if body.len().saturating_add(available as usize) > 1024 * 1024 {
            return Err("Usage response exceeded the 1 MB safety limit.".to_string());
        }
        let start = body.len();
        body.resize(start + available as usize, 0);
        let mut read = 0u32;
        checked_bool(
            unsafe {
                WinHttpReadData(
                    request.0,
                    body[start..].as_mut_ptr().cast(),
                    available,
                    &mut read,
                )
            },
            "reading usage response",
        )?;
        body.truncate(start + read as usize);
    }

    Ok((status, body))
}

fn checked_bool(result: i32, operation: &str) -> Result<(), String> {
    if result == 0 {
        Err(last_error(operation))
    } else {
        Ok(())
    }
}

fn last_error(operation: &str) -> String {
    format!(
        "Failed while {operation}: {}",
        std::io::Error::last_os_error()
    )
}
