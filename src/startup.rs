use crate::registry::{KEY_QUERY_VALUE, KEY_SET_VALUE, Key, REG_SZ};
use crate::wide;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "QuotaTray";

pub fn is_enabled() -> bool {
    Key::open(RUN_KEY, KEY_QUERY_VALUE)
        .and_then(|key| key.query(VALUE_NAME, &mut []))
        .is_some()
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let key = Key::create(RUN_KEY, KEY_SET_VALUE)
        .ok_or_else(|| "Windows startup settings could not be opened.".to_string())?;
    if !enabled {
        key.delete(VALUE_NAME);
        return Ok(());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("Executable path unavailable: {error}"))?;
    let command = wide(format!("\"{}\"", executable.display()))
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    if key.set(VALUE_NAME, REG_SZ, &command) {
        Ok(())
    } else {
        Err("Windows startup setting could not be saved.".to_string())
    }
}
