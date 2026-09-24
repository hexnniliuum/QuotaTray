use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use crate::model::Provider;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Source {
    #[default]
    Auto,
    Windows,
    Wsl,
}

impl Source {
    pub const ALL: [Self; 3] = [Self::Auto, Self::Windows, Self::Wsl];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (WSL, then Windows)",
            Self::Windows => "Windows",
            Self::Wsl => "WSL",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Windows => "windows",
            Self::Wsl => "wsl",
        }
    }

    pub fn candidates(self) -> &'static [Self] {
        match self {
            Self::Auto => &[Self::Wsl, Self::Windows],
            Self::Windows => &[Self::Windows],
            Self::Wsl => &[Self::Wsl],
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderSettings {
    pub source: Source,
    pub distro: Option<String>,
    pub windows_config_dir: Option<String>,
    pub wsl_config_dir: Option<String>,
}

impl ProviderSettings {
    pub fn windows_dir(&self, provider: Provider) -> Result<PathBuf, String> {
        if let Some(path) = &self.windows_config_dir {
            return Ok(PathBuf::from(path));
        }
        let variable = match provider {
            Provider::Claude => "CLAUDE_CONFIG_DIR",
            Provider::Codex => "CODEX_HOME",
        };
        if let Some(path) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err(format!("{variable} must be an absolute Windows path."));
            }
            return Ok(path);
        }
        std::env::var_os("USERPROFILE")
            .map(|home| PathBuf::from(home).join(default_directory(provider)))
            .ok_or_else(|| "Windows user profile was not found.".to_string())
    }
}

pub fn default_directory(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => ".claude",
        Provider::Codex => ".codex",
    }
}

#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub providers: [ProviderSettings; Provider::COUNT],
}

impl Settings {
    pub fn load() -> Result<Self, String> {
        match fs::read_to_string(settings_path()?) {
            Ok(content) => Self::parse(&content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => {
                Err("Cannot read sources.json. Open Sources > Edit advanced settings.".into())
            }
        }
    }

    fn parse(content: &str) -> Result<Self, String> {
        let invalid =
            || "Invalid sources.json. Check the source names and absolute paths.".to_string();
        let root: Value =
            serde_json::from_str(content.trim_start_matches('\u{feff}')).map_err(|_| invalid())?;
        let object = root.as_object().ok_or_else(invalid)?;
        if object
            .keys()
            .any(|key| Provider::ALL.iter().all(|provider| provider.key() != key.as_str()))
        {
            return Err(invalid());
        }
        let mut settings = Self::default();
        for provider in Provider::ALL {
            let Some(value) = object.get(provider.key()) else {
                continue;
            };
            let fields = value.as_object().ok_or_else(invalid)?;
            if fields.keys().any(|key| {
                !["source", "distro", "windows_config_dir", "wsl_config_dir"]
                    .contains(&key.as_str())
            }) {
                return Err(invalid());
            }
            let optional = |key: &str| -> Result<Option<String>, String> {
                match fields.get(key) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(value)) if !value.contains(['\0', '\r', '\n']) => {
                        Ok((!value.is_empty()).then(|| value.clone()))
                    }
                    _ => Err(invalid()),
                }
            };
            let source = match optional("source")?.as_deref() {
                None | Some("auto") => Source::Auto,
                Some("windows") => Source::Windows,
                Some("wsl") => Source::Wsl,
                _ => return Err(invalid()),
            };
            let config = ProviderSettings {
                source,
                distro: optional("distro")?,
                windows_config_dir: optional("windows_config_dir")?,
                wsl_config_dir: optional("wsl_config_dir")?,
            };
            if config
                .windows_config_dir
                .as_ref()
                .is_some_and(|path| !PathBuf::from(path).is_absolute())
                || config
                    .wsl_config_dir
                    .as_ref()
                    .is_some_and(|path| !path.starts_with('/'))
                || config
                    .distro
                    .as_ref()
                    .is_some_and(|name| name.starts_with('-'))
            {
                return Err(invalid());
            }
            settings.providers[provider.index()] = config;
        }
        Ok(settings)
    }

    pub fn save(&self) -> Result<(), String> {
        let path = settings_path()?;
        let mut root = json!({});
        for provider in Provider::ALL {
            let config = &self.providers[provider.index()];
            root[provider.key()] = json!({
                "source": config.source.key(),
                "distro": config.distro,
                "windows_config_dir": config.windows_config_dir,
                "wsl_config_dir": config.wsl_config_dir,
            });
        }
        fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| fs::write(path, serde_json::to_string_pretty(&root).unwrap()))
            .map_err(|_| "Could not save sources.json.".to_string())
    }
}

pub fn settings_path() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .map(|root| PathBuf::from(root).join("QuotaTray").join("sources.json"))
        .ok_or_else(|| "LOCALAPPDATA was not found.".to_string())
}

// Auto retries the complete read, including authentication, in the next environment.
// Explicit selection never crosses into another environment's account.
pub fn try_sources<T>(
    source: Source,
    mut read: impl FnMut(Source) -> Result<T, String>,
) -> Result<T, String> {
    let mut errors = Vec::new();
    for &candidate in source.candidates() {
        match read(candidate) {
            Ok(value) => return Ok(value),
            Err(error) => errors.push(format!("{}: {error}", candidate.label())),
        }
    }
    Err(errors.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_source_never_uses_another_account() {
        let mut calls = Vec::new();
        let result: Result<(), _> = try_sources(Source::Wsl, |source| {
            calls.push(source);
            Err("Login expired".into())
        });
        assert!(result.is_err());
        assert_eq!(calls, [Source::Wsl]);
    }

    #[test]
    fn auto_recovers_from_expired_wsl_login() {
        let mut calls = Vec::new();
        let result = try_sources(Source::Auto, |source| {
            calls.push(source);
            if source == Source::Wsl {
                Err("Login expired".into())
            } else {
                Ok(42)
            }
        });
        assert_eq!(result, Ok(42));
        assert_eq!(calls, [Source::Wsl, Source::Windows]);
    }

    #[test]
    fn malformed_settings_fail_instead_of_selecting_another_account() {
        for content in [
            r#"{"codex":{"source":"windwos"}}"#,
            r#"{"codex":{"windows_config_dir":"relative"}}"#,
            r#"{"claude":{"wsl_config_dir":"~/custom"}}"#,
            r#"{"codxe":{}}"#,
            "null",
        ] {
            assert!(Settings::parse(content).is_err());
        }
    }

    #[test]
    fn accepts_custom_paths_and_optional_distro() {
        let settings = Settings::parse(r#"{"codex":{"source":"wsl","distro":"Ubuntu","wsl_config_dir":"/home/example/custom config","windows_config_dir":"C:\\Example\\Codex"}}"#).unwrap();
        assert_eq!(settings.providers[1].source, Source::Wsl);
        assert_eq!(settings.providers[1].distro.as_deref(), Some("Ubuntu"));
        assert_eq!(settings.providers[0], ProviderSettings::default());
    }
}
