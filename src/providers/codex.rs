use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::model::{
    Provider, ProviderSnapshot, ResetCredit, ResetCredits, UsageWindow, now_unix,
    parse_rfc3339_unix,
};
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
        let command = source_command(config, source)?;
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

fn source_command(config: &ProviderSettings, source: Source) -> Result<Command, String> {
    if source == Source::Wsl {
        Ok(wsl::codex_command(config))
    } else {
        let mut command = windows_command();
        command.env("CODEX_HOME", config.windows_dir(Provider::Codex)?);
        Ok(command)
    }
}

#[derive(Debug, PartialEq)]
pub enum ResetOutcome {
    Applied,
    AlreadyApplied,
    NothingToReset,
    NoCredit,
}

/// One connection is retained throughout selection and redemption. Never fall
/// back to another source after the user has selected a credit.
pub struct ResetSession {
    server: AppServer,
    pub snapshot: ProviderSnapshot,
    pub source: Source,
    account: Value,
    workspace: Value,
}

impl ResetSession {
    pub fn open(config: &ProviderSettings) -> Result<Self, String> {
        try_sources(config.source, |source| {
            let mut server = AppServer::connect(source_command(config, source)?)?;
            let account = server.request("account/read", json!({"refreshToken": false}))?;
            let workspace = account["result"]["workspaceRouting"].clone();
            let account = account
                .get("result")
                .and_then(|r| r.get("account"))
                .filter(|a| !a.is_null())
                .cloned()
                .ok_or("Sign in to Codex in the selected environment.")?;
            let snapshot =
                parse_app_server_response(&server.request("account/rateLimits/read", json!({}))?)?;
            Ok(Self {
                server,
                snapshot,
                source,
                account,
                workspace,
            })
        })
    }

    pub fn account_label(&self) -> &str {
        self.account
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or("Signed-in Codex account")
    }

    pub fn consume(&mut self, credit_id: Option<&str>, key: &str) -> Result<ResetOutcome, String> {
        let account = self
            .server
            .request("account/read", json!({"refreshToken": false}))?;
        if account.get("result").and_then(|r| r.get("account")) != Some(&self.account)
            || account["result"]["workspaceRouting"] != self.workspace
        {
            return Err(
                "The Codex account changed or its workspace changed. Reopen Available resets."
                    .into(),
            );
        }
        let latest = self.server.request("account/rateLimits/read", json!({}))?;
        let credits = parse_reset_credits(&latest["result"])
            .ok_or("Reset availability is unknown. Update Codex and try again.")?;
        if credits.available_count == 0 {
            return Ok(ResetOutcome::NoCredit);
        }
        if let Some(id) = credit_id
            && !credits
                .choices(now_unix())
                .iter()
                .any(|credit| credit.id == id)
        {
            return Err("That reset is no longer available. Reopen Available resets.".into());
        }
        let mut params = json!({"idempotencyKey": key});
        if let Some(id) = credit_id {
            params["creditId"] = json!(id);
        }
        let response = self.server.request("account/rateLimitResetCredit/consume", params)
            .map_err(|_| "The reset result could not be confirmed. Check Codex usage before trying another reset.".to_string())?;
        let outcome = reset_outcome(&response)?;
        // The consume response does not contain the resulting usage windows.
        let refreshed = self.server.request("account/rateLimits/read", json!({}));
        if let Ok(snapshot) = refreshed.and_then(|value| parse_app_server_response(&value)) {
            self.snapshot = snapshot;
        }
        Ok(outcome)
    }
}

fn reset_outcome(response: &Value) -> Result<ResetOutcome, String> {
    match response["result"]["outcome"].as_str() {
        Some("reset") => Ok(ResetOutcome::Applied),
        Some("alreadyRedeemed") => Ok(ResetOutcome::AlreadyApplied),
        Some("nothingToReset") => Ok(ResetOutcome::NothingToReset),
        Some("noCredit") => Ok(ResetOutcome::NoCredit),
        _ => Err(
            "The reset result was not recognized. Check Codex usage before trying another reset."
                .into(),
        ),
    }
}

struct AppServer {
    _child: ManagedChild,
    stdin: ChildStdin,
    responses: mpsc::Receiver<Value>,
    next_id: i64,
}

impl AppServer {
    fn connect(mut command: Command) -> Result<Self, String> {
        command
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        let mut child = ManagedChild::spawn(&mut command)?;
        let stdin = child.stdin.take().ok_or("Codex input pipe failed.")?;
        let stdout = child.stdout.take().ok_or("Codex output pipe failed.")?;
        let (tx, responses) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && value.get("id").is_some()
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut server = Self {
            _child: child,
            stdin,
            responses,
            next_id: 1,
        };
        server.request(
            "initialize",
            json!({
                "clientInfo": {"name": "quota-tray", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )?;
        write_json_line(
            &mut server.stdin,
            &json!({"method": "initialized", "params": {}}),
        )?;
        Ok(server)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        write_json_line(
            &mut self.stdin,
            &json!({"id": id, "method": method, "params": params}),
        )?;
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            let response = self
                .responses
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| "Codex request timed out or the connection closed.".to_string())?;
            if response.get("id").and_then(Value::as_i64) != Some(id) {
                continue;
            }
            return checked_response(response);
        }
    }
}

fn checked_response(response: Value) -> Result<Value, String> {
    if response.get("error").is_some_and(|error| !error.is_null()) {
        return Err("Codex rejected the request. Check your sign-in and update Codex if this feature is unavailable.".into());
    }
    Ok(response)
}

fn read_from_app_server(command: Command) -> Result<ProviderSnapshot, String> {
    parse_app_server_response(
        &AppServer::connect(command)?.request("account/rateLimits/read", json!({}))?,
    )
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, value)
        .map_err(|error| format!("Could not encode Codex request: {error}"))?;
    writer
        .write_all(b"\n")
        .and_then(|_| writer.flush())
        .map_err(|error| format!("Could not send Codex request: {error}"))
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
            snapshot.reset_credits = parse_reset_credits(result);
            return Ok(snapshot);
        }
    }
    if let Some(credits) = parse_reset_credits(result) {
        let mut snapshot = ProviderSnapshot::empty(Provider::Codex);
        snapshot.reset_credits = Some(credits);
        snapshot.last_updated_unix = Some(now_unix());
        return Ok(snapshot);
    }
    Err("Codex returned no subscription usage windows.".to_string())
}

fn parse_reset_credits(result: &Value) -> Option<ResetCredits> {
    let value = result.get("rateLimitResetCredits")?;
    Some(ResetCredits {
        available_count: value.get("availableCount")?.as_u64()?,
        credits: value.get("credits").and_then(Value::as_array).map(|rows| {
            rows.iter()
                .filter_map(|row| {
                    if row.get("status")?.as_str()? != "available"
                        || row.get("resetType")?.as_str()? != "codexRateLimits"
                    {
                        return None;
                    }
                    let id = row.get("id")?.as_str()?.to_string();
                    if id.is_empty() {
                        return None;
                    }
                    Some(ResetCredit {
                        id,
                        title: row
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("Rate-limit reset")
                            .to_string(),
                        description: row
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("Reset an eligible Codex usage window.")
                            .to_string(),
                        expires_at: row.get("expiresAt").and_then(Value::as_i64),
                        expiry_known: row
                            .get("expiresAt")
                            .is_some_and(|v| v.is_null() || v.as_i64().is_some()),
                    })
                })
                .collect()
        }),
    })
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
    let mut files = session_files(root);
    files.sort_by_cached_key(|path| {
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

fn session_files(root: &Path) -> Vec<PathBuf> {
    let mut output = Vec::new();
    for year in subdirectories(root) {
        for month in subdirectories(&year) {
            for day in subdirectories(&month) {
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

fn subdirectories(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
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
    fn reset_details_keep_count_filter_unavailable_and_sort_expiry() {
        let credits = parse_reset_credits(&json!({"rateLimitResetCredits": {
            "availableCount": 9,
            "credits": [
                {"id":"unknown", "resetType":"codexRateLimits", "status":"available", "expiresAt":null},
                {"id":"later", "resetType":"codexRateLimits", "status":"available", "expiresAt":300},
                {"id":"first", "resetType":"codexRateLimits", "status":"available", "expiresAt":200},
                {"id":"expired", "resetType":"codexRateLimits", "status":"available", "expiresAt":100},
                {"id":"used", "resetType":"codexRateLimits", "status":"redeemed", "expiresAt":400},
                {"id":"other", "resetType":"other", "status":"available", "expiresAt":400}
            ]
        }})).unwrap();
        assert_eq!(credits.available_count, 9);
        assert_eq!(
            credits
                .choices(100)
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "later", "unknown"]
        );
    }

    #[test]
    fn reset_availability_distinguishes_missing_count_only_and_empty_details() {
        assert!(parse_reset_credits(&json!({})).is_none());
        assert!(parse_reset_credits(&json!({"rateLimitResetCredits":null})).is_none());
        let count_only = parse_reset_credits(
            &json!({"rateLimitResetCredits": {"availableCount":2,"credits":null}}),
        )
        .unwrap();
        assert!(count_only.credits.is_none());
        let empty = parse_reset_credits(
            &json!({"rateLimitResetCredits": {"availableCount":0,"credits":[]}}),
        )
        .unwrap();
        assert!(empty.credits.unwrap().is_empty());
        let snapshot = parse_app_server_response(
            &json!({"result":{"rateLimitResetCredits":{"availableCount":2}}}),
        )
        .unwrap();
        assert_eq!(snapshot.reset_credits.unwrap().available_count, 2);
    }

    #[test]
    fn redemption_outcomes_are_explicit_and_unknown_is_not_success() {
        for outcome in ["reset", "alreadyRedeemed", "nothingToReset", "noCredit"] {
            assert!(reset_outcome(&json!({"result":{"outcome":outcome}})).is_ok());
        }
        assert!(reset_outcome(&json!({"result":{"outcome":"futureOutcome"}})).is_err());
    }

    #[test]
    fn reset_session_checks_account_sends_selected_credit_and_refreshes() {
        let root =
            std::env::temp_dir().join(format!("quota-tray-reset-mock-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("server.ps1");
        fs::write(&script, r#"
while ($line = [Console]::ReadLine()) {
    $request = $line | ConvertFrom-Json
    if ($request.method -eq 'initialized') { continue }
    $result = @{}
    switch ($request.method) {
        'account/read' { $result = @{ account = @{ type = 'chatgpt'; email = 'test@example.invalid' } } }
        'account/rateLimits/read' {
            $result = @{ rateLimits = @{ primary = @{ usedPercent = 12; windowDurationMins = 300 } };
                rateLimitResetCredits = @{ availableCount = 1; credits = @(@{ id = 'selected'; resetType = 'codexRateLimits'; status = 'available' }) } }
        }
        'account/rateLimitResetCredit/consume' {
            if ($request.params.creditId -ne 'selected' -or $request.params.idempotencyKey -ne 'stable-test-key') { exit 1 }
            $result = @{ outcome = 'reset' }
        }
    }
    [Console]::WriteLine((@{ id = $request.id; result = $result } | ConvertTo-Json -Depth 10 -Compress))
}
"#).unwrap();
        let mut command = Command::new("powershell.exe");
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script);
        let mut session = ResetSession {
            server: AppServer::connect(command).unwrap(),
            snapshot: ProviderSnapshot::empty(Provider::Codex),
            source: Source::Windows,
            account: json!({"type":"chatgpt", "email":"test@example.invalid"}),
            workspace: Value::Null,
        };
        assert!(
            session
                .consume(Some("missing"), "stable-test-key")
                .unwrap_err()
                .contains("no longer available")
        );
        assert_eq!(
            session
                .consume(Some("selected"), "stable-test-key")
                .unwrap(),
            ResetOutcome::Applied
        );
        assert_eq!(
            session.snapshot.session.as_ref().unwrap().used_percent,
            12.0
        );
        session.account = json!({"type":"chatgpt", "email":"changed@example.invalid"});
        assert!(
            session
                .consume(Some("selected"), "stable-test-key")
                .unwrap_err()
                .contains("account changed")
        );
        drop(session);
        fs::remove_dir_all(root).unwrap();
    }

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
    fn session_fallback_finds_resumed_sessions_in_older_directories() {
        let root = std::env::temp_dir().join(format!("quota-tray-resumed-{}", std::process::id()));
        for directory in [
            "2024/01/01",
            "2024/01/02",
            "2024/01/03",
            "2024/01/04",
            "2024/02/01",
            "2024/03/01",
            "2025/01/01",
            "2026/01/01",
        ] {
            let day = root.join(directory);
            fs::create_dir_all(&day).unwrap();
            let file = day.join("rollout-synthetic.jsonl");
            fs::write(
                &file,
                json!({
                    "type": "token_count",
                    "rate_limits": {"primary": {"used_percent": 10, "window_minutes": 300}}
                })
                .to_string(),
            )
            .unwrap();
            fs::File::options()
                .write(true)
                .open(file)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000))
                .unwrap();
        }
        fs::write(
            root.join("2024/01/01/rollout-synthetic.jsonl"),
            json!({
                "type": "token_count",
                "rate_limits": {"primary": {"used_percent": 37, "window_minutes": 300}}
            })
            .to_string(),
        )
        .unwrap();
        let result = read_from_session_files(&root);
        fs::remove_dir_all(&root).unwrap();
        let snapshot = result.unwrap();
        assert_eq!(snapshot.session.unwrap().used_percent, 37.0);
        assert!(snapshot.from_session_history);
    }

    #[test]
    fn app_server_error_details_are_not_exposed() {
        let response = json!({"id": 2, "error": {"message": "private account detail"}});
        let error = checked_response(response).unwrap_err();
        assert!(!error.contains("private account detail"));
        assert!(error.contains("sign-in"));
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
