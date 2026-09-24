use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::model::{Provider, ProviderSnapshot, UsageWindow, now_unix, parse_rfc3339_unix};
use crate::process::ManagedChild;
use crate::settings::{ProviderSettings, Source, try_sources};
use crate::wsl;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug)]
struct RawWindow {
    used_percent: f64,
    duration_minutes: Option<i64>,
    resets_at_unix: Option<i64>,
}

pub fn refresh(config: &ProviderSettings) -> Result<ProviderSnapshot, String> {
    try_sources(config.source, |source| {
        let command = match source {
            Source::Wsl => wsl::codex_command(config),
            _ => {
                let mut command = windows_command();
                command.env("CODEX_HOME", config.windows_dir(Provider::Codex)?);
                command
            }
        };
        live_or_sessions(
            || read_from_app_server(command),
            || {
                let root = match source {
                    Source::Wsl => wsl::config_path(config, Provider::Codex)?,
                    _ => config.windows_dir(Provider::Codex)?,
                };
                read_from_session_files(&root.join("sessions"))
            },
        )
    })
}

fn windows_command() -> Command {
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if !directory.is_absolute() {
                continue;
            }
            for name in ["codex.exe", "codex.cmd"] {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Command::new(candidate);
                }
            }
        }
    }
    Command::new("codex")
}

fn live_or_sessions(
    live: impl FnOnce() -> Result<ProviderSnapshot, String>,
    sessions: impl FnOnce() -> Result<ProviderSnapshot, String>,
) -> Result<ProviderSnapshot, String> {
    live().or_else(|live_error| {
        sessions().map_err(|session_error| {
            format!("{live_error} {session_error} Sign in to Codex in the selected environment.")
        })
    })
}

fn read_from_app_server(mut command: Command) -> Result<ProviderSnapshot, String> {
    command
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = ManagedChild::spawn(&mut command)?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Codex input pipe failed.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Codex output pipe failed.".to_string())?;
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let result = (|| {
            write_json_line(
                &mut stdin,
                &json!({
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "clientInfo": {"name": "quota-tray", "version": env!("CARGO_PKG_VERSION")},
                        "capabilities": {"experimentalApi": true}
                    }
                }),
            )?;
            read_response(&mut reader, 1)?;
            write_json_line(&mut stdin, &json!({"method": "initialized", "params": {}}))?;
            write_json_line(
                &mut stdin,
                &json!({"id": 2, "method": "account/rateLimits/read", "params": {}}),
            )?;
            let response = read_response(&mut reader, 2)?;
            parse_app_server_response(&response)
        })();
        let _ = result_tx.send(result);
    });

    let result = result_rx.recv_timeout(Duration::from_secs(12));
    drop(child);
    result.map_err(|_| "Codex usage query timed out.".to_string())?
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| format!("Could not encode Codex request: {error}"))?;
    writer
        .write_all(b"\n")
        .and_then(|_| writer.flush())
        .map_err(|error| format!("Could not send Codex request: {error}"))
}

fn read_response(reader: &mut impl BufRead, expected_id: i64) -> Result<Value, String> {
    for _ in 0..30 {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Err("Codex app server closed unexpectedly.".to_string());
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(Value::as_i64) == Some(expected_id) {
            if value.get("error").is_some_and(|error| !error.is_null()) {
                return Err(
                    "Codex rejected the usage request. Sign in again in the selected environment."
                        .into(),
                );
            }
            return Ok(value);
        }
    }
    Err("Codex app server did not return usage data.".to_string())
}

fn parse_app_server_response(response: &Value) -> Result<ProviderSnapshot, String> {
    let result = response
        .get("result")
        .ok_or_else(|| "Codex usage response had no result.".to_string())?;
    let mut candidates = Vec::new();
    if let Some(limit) = result.get("rateLimits").filter(|value| !value.is_null()) {
        candidates.push(limit);
    }
    if let Some(limits) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        candidates.extend(limits.values());
    }
    for candidate in candidates {
        let windows = [candidate.get("primary"), candidate.get("secondary")]
            .into_iter()
            .flatten()
            .filter_map(parse_camel_window)
            .collect::<Vec<_>>();
        if !windows.is_empty() {
            let mut snapshot = snapshot_from_windows(windows);
            snapshot.last_updated_unix = Some(now_unix());
            return Ok(snapshot);
        }
    }
    Err("Codex returned no subscription usage windows.".to_string())
}

fn parse_camel_window(value: &Value) -> Option<RawWindow> {
    if value.is_null() {
        return None;
    }
    Some(RawWindow {
        used_percent: value.get("usedPercent")?.as_f64()?,
        duration_minutes: value.get("windowDurationMins").and_then(Value::as_i64),
        resets_at_unix: value.get("resetsAt").and_then(Value::as_i64),
    })
}

fn read_from_session_files(root: &Path) -> Result<ProviderSnapshot, String> {
    let mut files = recent_session_files(root);
    files.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });

    for path in files.into_iter().rev().take(5) {
        let Ok(content) = fs::read_to_string(path) else {
            continue;
        };
        if let Some(snapshot) = parse_session_content(&content) {
            return Ok(snapshot);
        }
    }
    Err("No current usage was found in recent sessions.".to_string())
}

fn parse_session_content(content: &str) -> Option<ProviderSnapshot> {
    for line in content.lines().rev() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let payload = if event.get("type").and_then(Value::as_str) == Some("event_msg") {
            event.get("payload").unwrap_or(&Value::Null)
        } else {
            &event
        };
        if payload.get("type").and_then(Value::as_str) != Some("token_count") {
            continue;
        }
        let Some(rate_limits) = payload.get("rate_limits") else {
            continue;
        };
        let windows = [rate_limits.get("primary"), rate_limits.get("secondary")]
            .into_iter()
            .flatten()
            .filter_map(parse_snake_window)
            .filter(|window| window.resets_at_unix.is_none_or(|reset| reset > now_unix()))
            .collect::<Vec<_>>();
        if !windows.is_empty() {
            let mut snapshot = snapshot_from_windows(windows);
            snapshot.from_session_history = true;
            snapshot.last_updated_unix = event
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_unix);
            return Some(snapshot);
        }
    }
    None
}

fn recent_session_files(root: &Path) -> Vec<PathBuf> {
    let mut output = Vec::new();
    for year in sorted_subdirectories(root).into_iter().rev().take(2) {
        for month in sorted_subdirectories(&year).into_iter().rev().take(2) {
            for day in sorted_subdirectories(&month).into_iter().rev().take(3) {
                let Ok(entries) = fs::read_dir(day) else {
                    continue;
                };
                output.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            name.starts_with("rollout-") && name.ends_with(".jsonl")
                        })
                }));
            }
        }
    }
    output
}

fn sorted_subdirectories(root: &Path) -> Vec<PathBuf> {
    let mut directories = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    directories
}

fn parse_snake_window(value: &Value) -> Option<RawWindow> {
    if value.is_null() {
        return None;
    }
    Some(RawWindow {
        used_percent: value.get("used_percent")?.as_f64()?,
        duration_minutes: value.get("window_minutes").and_then(Value::as_i64),
        resets_at_unix: value.get("resets_at").and_then(Value::as_i64),
    })
}

fn snapshot_from_windows(mut windows: Vec<RawWindow>) -> ProviderSnapshot {
    windows.sort_by_key(|window| window.duration_minutes.unwrap_or(i64::MAX));
    let mut snapshot = ProviderSnapshot::empty(Provider::Codex);
    if windows.len() == 1 {
        let window = windows.remove(0);
        if window
            .duration_minutes
            .is_some_and(|minutes| minutes > 24 * 60)
        {
            snapshot.weekly = Some(to_usage_window(window, "Weekly"));
        } else {
            snapshot.session = Some(to_usage_window(window, "Session"));
        }
    } else {
        let session = windows.remove(0);
        let weekly = windows.pop().unwrap();
        snapshot.session = Some(to_usage_window(session, "Session"));
        snapshot.weekly = Some(to_usage_window(weekly, "Weekly"));
    }
    snapshot
}

fn to_usage_window(window: RawWindow, fallback_label: &str) -> UsageWindow {
    let label = match window.duration_minutes {
        Some(300) => "Session (5-hour)".to_string(),
        Some(10_080) => "Weekly".to_string(),
        Some(minutes) if minutes % 1_440 == 0 => format!("{}-day window", minutes / 1_440),
        Some(minutes) if minutes % 60 == 0 => format!("{}-hour window", minutes / 60),
        Some(minutes) => format!("{minutes}-minute window"),
        None => fallback_label.to_string(),
    };
    UsageWindow::new(label, window.used_percent, window.resets_at_unix)
        .with_duration_secs(window.duration_minutes.map(|minutes| minutes * 60))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_live_usage_through_a_windows_cmd_launcher() {
        let root =
            std::env::temp_dir().join(format!("quota-tray-cli with spaces-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let launcher = root.join("codex.cmd");
        let server = root.join("server.ps1");
        fs::write(&launcher, "@echo off\r\npowershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"%~dp0server.ps1\"\r\n").unwrap();
        fs::write(&server, r#"
$null = [Console]::ReadLine()
[Console]::WriteLine('{"id":1,"result":{}}')
$null = [Console]::ReadLine()
$null = [Console]::ReadLine()
[Console]::WriteLine('{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":23,"windowDurationMins":300}}}}')
Start-Sleep -Seconds 30
"#).unwrap();
        let result = read_from_app_server(Command::new(&launcher));
        fs::remove_file(launcher).unwrap();
        fs::remove_file(server).unwrap();
        fs::remove_dir(root).unwrap();
        assert_eq!(result.unwrap().session.unwrap().used_percent, 23.0);
    }

    #[test]
    fn windows_session_fallback_returns_saved_usage_when_cli_fails() {
        let root = std::env::temp_dir().join(format!("quota-tray-sessions-{}", std::process::id()));
        let day = root.join("2026/01/01");
        fs::create_dir_all(&day).unwrap();
        let file = day.join("rollout-synthetic.jsonl");
        fs::write(
            &file,
            json!({
                "timestamp": "2026-01-01T10:00:00Z", "type": "token_count",
                "rate_limits": {"primary": {"used_percent": 37, "window_minutes": 300}}
            })
            .to_string(),
        )
        .unwrap();
        let snapshot = live_or_sessions(
            || Err("CLI unavailable".into()),
            || read_from_session_files(&root),
        )
        .unwrap();
        assert_eq!(snapshot.session.unwrap().used_percent, 37.0);
        assert!(snapshot.from_session_history);
        fs::remove_file(file).unwrap();
        fs::remove_dir(&day).unwrap();
        fs::remove_dir(day.parent().unwrap()).unwrap();
        fs::remove_dir(root.join("2026")).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn app_server_error_details_are_not_exposed() {
        let response = b"{\"id\":2,\"error\":{\"message\":\"private account detail\"}}\n";
        let error = read_response(&mut &response[..], 2).unwrap_err();
        assert!(!error.contains("private account detail"));
        assert!(error.contains("Sign in"));
    }

    #[test]
    fn rereading_saved_usage_preserves_measurement_time() {
        let content = json!({
            "timestamp": "2026-01-01T10:00:00Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "rate_limits": {"primary": {"used_percent": 40, "window_minutes": 300}}
            }
        })
        .to_string();
        let recorded = parse_rfc3339_unix("2026-01-01T10:00:00Z").unwrap();
        for _ in 0..2 {
            let snapshot = parse_session_content(&content).unwrap();
            assert!(snapshot.from_session_history);
            assert_eq!(snapshot.session.as_ref().unwrap().used_percent, 40.0);
            assert_eq!(snapshot.last_updated_unix, Some(recorded));
            assert_eq!(
                snapshot.freshness_label(recorded + 3600),
                "Saved session usage · recorded 1h ago"
            );
        }
    }

    #[test]
    fn saved_usage_without_valid_timestamp_does_not_claim_freshness() {
        for timestamp in [Value::Null, json!("invalid")] {
            let content = json!({
                "timestamp": timestamp,
                "type": "token_count",
                "rate_limits": {"primary": {"used_percent": 40, "window_minutes": 300}}
            })
            .to_string();
            let snapshot = parse_session_content(&content).unwrap();
            assert_eq!(snapshot.last_updated_unix, None);
            assert_eq!(
                snapshot.freshness_label(now_unix()),
                "Saved session usage · recording time unknown"
            );
        }
    }

    #[test]
    fn live_usage_is_marked_as_updated_now() {
        let before = now_unix();
        let snapshot = parse_app_server_response(&json!({
            "result": {"rateLimits": {"primary": {
                "usedPercent": 40, "windowDurationMins": 300
            }}}
        }))
        .unwrap();
        assert!(!snapshot.from_session_history);
        assert!(snapshot.last_updated_unix.unwrap() >= before);
        assert_eq!(snapshot.freshness_label(now_unix()), "Updated just now");
    }

    #[test]
    fn maps_shorter_and_longer_codex_windows_by_duration() {
        let snapshot = snapshot_from_windows(vec![
            RawWindow {
                used_percent: 66.0,
                duration_minutes: Some(10_080),
                resets_at_unix: Some(20_000),
            },
            RawWindow {
                used_percent: 11.0,
                duration_minutes: Some(300),
                resets_at_unix: Some(10_000),
            },
        ]);
        assert_eq!(snapshot.session.unwrap().used_percent, 11.0);
        assert_eq!(snapshot.weekly.unwrap().used_percent, 66.0);
    }
}
