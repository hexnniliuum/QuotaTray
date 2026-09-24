use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
};
use windows::Win32::System::SystemInformation::GetLocalTime;

const LOG_DIRECTORY: &str = "QuotaTray";
const LOG_FILE: &str = "quota-tray.log";
const LOG_PERIOD_MARKER: &str = "log-period.marker";
const RUN_MARKER: &str = "running.marker";
const LOG_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

static LOGGER: OnceLock<Logger> = OnceLock::new();

struct Logger {
    state: Mutex<LogState>,
    marker_path: PathBuf,
    period_marker_path: PathBuf,
}

struct LogState {
    file: File,
    period_started: SystemTime,
}

pub fn init() -> io::Result<PathBuf> {
    let directory = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(LOG_DIRECTORY);
    fs::create_dir_all(&directory)?;

    let log_path = directory.join(LOG_FILE);
    let mut file = open_log(&log_path)?;

    let now = SystemTime::now();
    let period_marker_path = directory.join(LOG_PERIOD_MARKER);
    let metadata = file.metadata()?;
    let period_start = read_period_start(&period_marker_path)
        .or_else(|| metadata.created().ok())
        .or_else(|| metadata.modified().ok());
    let retention_expired = period_start.is_some_and(|started| period_expired(started, now));
    let size_exceeded = metadata.len() > MAX_LOG_BYTES;
    if retention_expired || size_exceeded {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let reason = if retention_expired {
            "[log cleared after 24 hours]\r\n"
        } else {
            "[log truncated at 2 MiB]\r\n"
        };
        file.write_all(reason.as_bytes())?;
    }
    let period_started = match period_start {
        Some(started) if !retention_expired && !size_exceeded => started,
        _ => {
            write_period_start(&period_marker_path, now)?;
            now
        }
    };

    let marker_path = directory.join(RUN_MARKER);
    let previous_run = fs::read_to_string(&marker_path).ok();
    fs::write(
        &marker_path,
        format!(
            "pid={} started={} version={}\r\n",
            std::process::id(),
            timestamp(),
            env!("CARGO_PKG_VERSION")
        ),
    )?;

    LOGGER
        .set(Logger {
            state: Mutex::new(LogState {
                file,
                period_started,
            }),
            marker_path,
            period_marker_path,
        })
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "diagnostics already initialized",
            )
        })?;

    install_crash_handlers();
    event(
        "INFO",
        &format!(
            "process started; pid={}; version={}",
            std::process::id(),
            env!("CARGO_PKG_VERSION")
        ),
    );
    if let Some(previous_run) = previous_run {
        event(
            "WARN",
            &format!(
                "previous process did not record a clean shutdown; {}",
                previous_run.trim()
            ),
        );
    }
    Ok(log_path)
}

pub fn event(level: &str, message: &str) {
    let Some(logger) = LOGGER.get() else {
        return;
    };
    let Ok(mut state) = logger.state.try_lock() else {
        return;
    };
    reset_if_needed(&mut state, &logger.period_marker_path);
    let message = message.replace(['\r', '\n'], " ");
    let _ = state.file.seek(SeekFrom::End(0));
    let _ = writeln!(state.file, "{} [{level}] {message}", timestamp());
    let _ = state.file.flush();
}

pub fn shutdown() {
    event("INFO", "process stopped cleanly");
    if let Some(logger) = LOGGER.get() {
        let _ = fs::remove_file(&logger.marker_path);
    }
}

fn reset_if_needed(state: &mut LogState, period_marker_path: &Path) {
    let now = SystemTime::now();
    let retention_expired = period_expired(state.period_started, now);
    let size_exceeded = state
        .file
        .metadata()
        .is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES);
    if !retention_expired && !size_exceeded {
        return;
    }

    if state.file.set_len(0).is_err() {
        return;
    }
    let _ = state.file.seek(SeekFrom::Start(0));
    let reason = if retention_expired {
        "[log cleared after 24 hours]\r\n"
    } else {
        "[log truncated at 2 MiB]\r\n"
    };
    let _ = state.file.write_all(reason.as_bytes());
    state.period_started = now;
    let _ = write_period_start(period_marker_path, now);
}

fn read_period_start(path: &Path) -> Option<SystemTime> {
    let seconds = fs::read_to_string(path).ok()?.trim().parse::<u64>().ok()?;
    UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

fn write_period_start(path: &Path, started: SystemTime) -> io::Result<()> {
    let seconds = started
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    fs::write(path, seconds.to_string())
}

fn period_expired(started: SystemTime, now: SystemTime) -> bool {
    now.duration_since(started)
        .map_or(true, |elapsed| elapsed >= LOG_RETENTION)
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
}

fn install_crash_handlers() {
    std::panic::set_hook(Box::new(|panic_info| {
        let payload = panic_info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| {
                panic_info
                    .payload()
                    .downcast_ref::<String>()
                    .map(String::as_str)
            })
            .unwrap_or("<non-text panic>");
        let location = panic_info
            .location()
            .map(|location| {
                format!(
                    "{}:{}:{}",
                    location.file(),
                    location.line(),
                    location.column()
                )
            })
            .unwrap_or_else(|| "<unknown>".to_string());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        event(
            "FATAL",
            &format!("panic on thread {thread} at {location}: {payload}"),
        );
    }));
    unsafe {
        SetUnhandledExceptionFilter(Some(unhandled_exception));
    }
}

unsafe extern "system" fn unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
    let Some(info) = (unsafe { info.as_ref() }) else {
        event(
            "FATAL",
            "unhandled Windows exception; exception details unavailable",
        );
        return EXCEPTION_CONTINUE_SEARCH;
    };
    let Some(record) = (unsafe { info.ExceptionRecord.as_ref() }) else {
        event(
            "FATAL",
            "unhandled Windows exception; exception record unavailable",
        );
        return EXCEPTION_CONTINUE_SEARCH;
    };
    event(
        "FATAL",
        &format!(
            "unhandled Windows exception code=0x{:08X} address={:p}",
            record.ExceptionCode.0 as u32, record.ExceptionAddress
        ),
    );
    EXCEPTION_CONTINUE_SEARCH
}

fn timestamp() -> String {
    let time = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_period_expires_at_24_hours() {
        let started = UNIX_EPOCH + Duration::from_secs(1_000);
        assert!(!period_expired(
            started,
            started + LOG_RETENTION - Duration::from_secs(1)
        ));
        assert!(period_expired(started, started + LOG_RETENTION));
    }

    #[test]
    fn future_period_marker_is_treated_as_expired() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert!(period_expired(now + Duration::from_secs(1), now));
    }

    #[test]
    fn log_file_can_be_truncated() {
        let path = std::env::temp_dir().join(format!(
            "quota-tray-log-permissions-{}.log",
            std::process::id()
        ));
        let mut file = open_log(&path).unwrap();
        file.write_all(b"old diagnostics").unwrap();
        file.set_len(0).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
        drop(file);
        fs::remove_file(path).unwrap();
    }
}
