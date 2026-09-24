use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::model::Provider;
use crate::process::ManagedChild;
use crate::settings::{ProviderSettings, default_directory};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(8);
const SCRIPT_NAME: &str = "quota-tray";
const CODEX_SCRIPT: &str = r#"if [ -n "$1" ]; then export CODEX_HOME="$1"; fi; shift; exec codex "$@""#;
const CONFIG_PATH_SCRIPT: &str = r#"dir="$1"; if [ -z "$dir" ]; then case "$2" in .codex) dir="${CODEX_HOME:-$HOME/.codex}";; .claude) dir="${CLAUDE_CONFIG_DIR:-$HOME/.claude}";; esac; fi; case "$dir" in /*) wslpath -w "$dir";; *) exit 2;; esac"#;

fn base_command(distro: Option<&str>) -> Command {
    let mut command = Command::new("wsl.exe");
    if let Some(distro) = distro {
        command.args(["--distribution", distro]);
    }
    command.creation_flags(CREATE_NO_WINDOW);
    command
}

pub fn codex_command(config: &ProviderSettings) -> Command {
    let mut command = base_command(config.distro.as_deref());
    command.args([
        "--exec",
        "bash",
        "-lc",
        CODEX_SCRIPT,
        SCRIPT_NAME,
        config.wsl_config_dir.as_deref().unwrap_or(""),
    ]);
    command
}

pub fn config_path(config: &ProviderSettings, provider: Provider) -> Result<PathBuf, String> {
    let mut command = base_command(config.distro.as_deref());
    command.args([
        "--exec",
        "bash",
        "-lc",
        CONFIG_PATH_SCRIPT,
        SCRIPT_NAME,
        config.wsl_config_dir.as_deref().unwrap_or(""),
        default_directory(provider),
    ]);
    let output = output_timeout(command, DISCOVERY_TIMEOUT)?;
    parse_windows_path(&output)
}

// Keep discovery bounded even if a distro or login shell never finishes.
fn output_timeout(mut command: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    let mut child = ManagedChild::spawn(&mut command).map_err(|_| {
        "WSL unavailable. Choose Windows in Sources or install/start the selected distro."
            .to_string()
    })?;
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(64 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                return Err("WSL discovery timed out. Start the selected distro or choose Windows in Sources.".into());
            }
        }
    };
    if !status.success() {
        return Err(
            "WSL discovery failed. Check the distro, bash, and configuration directory in Sources."
                .into(),
        );
    }
    receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "WSL path output timed out.".to_string())?
        .map_err(|_| "WSL path output could not be read.".to_string())
}

fn parse_windows_path(output: &[u8]) -> Result<PathBuf, String> {
    let path = PathBuf::from(decode_console(output));
    if !path.is_absolute() {
        return Err(
            "WSL did not return an absolute Windows path. Check shell startup output.".to_string(),
        );
    }
    Ok(path)
}

fn decode_console(output: &[u8]) -> String {
    if output.len() >= 2 && output.chunks_exact(2).any(|pair| pair[1] == 0) {
        let words = output
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
            .trim_matches('\0')
            .trim()
            .to_string()
    } else {
        String::from_utf8_lossy(output).trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wslpath_output_without_personal_paths() {
        let path =
            parse_windows_path("\\\\wsl.localhost\\Ubuntu\\home\\example\\.claude\r\n".as_bytes())
                .unwrap();
        assert_eq!(
            path,
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\example\.claude")
        );
        assert!(parse_windows_path(b"relative").is_err());
    }

    #[test]
    fn decodes_utf16_wsl_errors() {
        let bytes = "Access is denied.\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(decode_console(&bytes), "Access is denied.");
    }

    #[test]
    fn discovery_kills_a_stalled_process() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        let start = Instant::now();
        assert!(
            output_timeout(command, Duration::from_millis(100))
                .unwrap_err()
                .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn custom_paths_are_passed_as_arguments_not_shell_code() {
        let config = ProviderSettings {
            distro: Some("Example Distro".into()),
            wsl_config_dir: Some("/tmp/config with ' quotes;$text".into()),
            ..Default::default()
        };
        let command = codex_command(&config);
        let args = command
            .get_args()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(args[1], "Example Distro");
        assert_eq!(args.last().unwrap(), "/tmp/config with ' quotes;$text");
    }
}
