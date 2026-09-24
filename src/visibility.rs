use crate::registry::{KEY_QUERY_VALUE, KEY_SET_VALUE, Key, REG_DWORD};

const SETTINGS_KEY: &str = "Software\\QuotaTray";
const VALUE_NAME: &str = "VisibleProviders";
const ALL_PROVIDERS: u8 = 0b11;

pub fn load() -> u8 {
    let mut data = [0u8; 4];
    let stored = Key::open(SETTINGS_KEY, KEY_QUERY_VALUE)
        .and_then(|key| key.query(VALUE_NAME, &mut data));
    let mask = data[0] & ALL_PROVIDERS;
    match stored {
        Some((REG_DWORD, 4)) if mask != 0 => mask,
        _ => ALL_PROVIDERS,
    }
}

pub fn save(mask: u8) -> Result<(), String> {
    let key = Key::create(SETTINGS_KEY, KEY_SET_VALUE)
        .ok_or_else(|| "Provider visibility settings could not be opened.".to_string())?;
    let value = u32::from(mask & ALL_PROVIDERS).to_le_bytes();
    if key.set(VALUE_NAME, REG_DWORD, &value) {
        Ok(())
    } else {
        Err("Provider visibility settings could not be saved.".to_string())
    }
}
