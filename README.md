# Quota Tray

Claude Code and Codex subscription usage in the Windows notification tray.
Left-click for the dashboard. Right-click for a refresh.
Reads logins from Windows and WSL.

## Setup

Windows 10 or 11, x64. Run `quota-tray.exe`.
Sign in to Claude Code or Codex.
For live Codex usage on Windows, `codex.exe` or `codex.cmd` has to be on PATH. 
Logs: `%LOCALAPPDATA%\QuotaTray\quota-tray.log`

## Build

You need Rust with the `x86_64-pc-windows-msvc` target and VS Build Tools.
Run `scripts/package.ps1`. It writes the ZIP to `dist`.

## License

MIT
