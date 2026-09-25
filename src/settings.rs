use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::model::Provider;
use crate::palette::{Rgb, TrayTheme, UsageColors};

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

#[derive(Clone, Debug)]
pub struct Settings {
    pub providers: [ProviderSettings; Provider::COUNT],
    pub appearance: Result<Appearance, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            providers: std::array::from_fn(|_| ProviderSettings::default()),
            appearance: Ok(Appearance::default()),
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self, String> {
        match fs::read_to_string(settings_path()?) {
            Ok(content) => Self::parse(&content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err("Cannot read config.json. Open Sources > Edit advanced settings.".into()),
        }
    }

    fn parse(content: &str) -> Result<Self, String> {
        let invalid =
            || "Invalid config.json. Check the source names and absolute paths.".to_string();
        let root: Value =
            serde_json::from_str(content.trim_start_matches('\u{feff}')).map_err(|_| invalid())?;
        let object = root.as_object().ok_or_else(invalid)?;
        if object.keys().any(|key| {
            key != "tray_theme"
                && Provider::ALL
                    .iter()
                    .all(|provider| provider.key() != key.as_str())
        }) {
            return Err(invalid());
        }
        let mut settings = Self::default();
        for provider in Provider::ALL {
            let Some(value) = object.get(provider.key()) else {
                continue;
            };
            let fields = value.as_object().ok_or_else(invalid)?;
            if fields.keys().any(|key| {
                ![
                    "source",
                    "distro",
                    "windows_config_dir",
                    "wsl_config_dir",
                    "colors",
                ]
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
        settings.appearance = Appearance::parse(&root);
        Ok(settings)
    }

    pub fn save(&self) -> Result<(), String> {
        let content = serde_json::to_string_pretty(&self.to_json()?).unwrap();
        let path = settings_path()?;
        fs::create_dir_all(path.parent().unwrap())
            .and_then(|_| fs::write(path, content))
            .map_err(|_| "Could not save config.json.".to_string())
    }

    fn to_json(&self) -> Result<Value, String> {
        let appearance = self.appearance.as_ref().map_err(Clone::clone)?;
        let mut root = json!({"tray_theme": match appearance.tray_theme {
            TrayTheme::Dark => "dark",
            TrayTheme::Light => "light",
        }});
        for provider in Provider::ALL {
            let config = &self.providers[provider.index()];
            let colors = appearance.colors[provider.index()];
            root[provider.key()] = json!({
                "source": config.source.key(),
                "distro": config.distro,
                "windows_config_dir": config.windows_config_dir,
                "wsl_config_dir": config.wsl_config_dir,
                "colors": {
                    "normal": format_color(colors.normal),
                    "warning": format_color(colors.warning),
                },
            });
        }
        Ok(root)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Appearance {
    pub tray_theme: TrayTheme,
    pub colors: [UsageColors; Provider::COUNT],
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            tray_theme: TrayTheme::default(),
            colors: Provider::ALL.map(UsageColors::for_provider),
        }
    }
}

impl Appearance {
    fn parse(root: &Value) -> Result<Self, String> {
        let tray_theme = match root.get("tray_theme") {
            None | Some(Value::Null) => TrayTheme::default(),
            Some(Value::String(value)) if value == "dark" => TrayTheme::Dark,
            Some(Value::String(value)) if value == "light" => TrayTheme::Light,
            _ => return Err("Invalid config.json: tray_theme must be light or dark.".into()),
        };
        let mut appearance = Self {
            tray_theme,
            ..Self::default()
        };
        for provider in Provider::ALL {
            if let Some(value) = root
                .get(provider.key())
                .and_then(|fields| fields.get("colors"))
                .filter(|value| !value.is_null())
            {
                let invalid_color = || {
                    format!(
                        "Invalid config.json: {}.colors must contain normal or warning colors in #RRGGBB format.",
                        provider.key()
                    )
                };
                let colors = value.as_object().ok_or_else(invalid_color)?;
                if colors
                    .keys()
                    .any(|key| !["normal", "warning"].contains(&key.as_str()))
                {
                    return Err(invalid_color());
                }
                let target = &mut appearance.colors[provider.index()];
                for (key, color) in [
                    ("normal", &mut target.normal),
                    ("warning", &mut target.warning),
                ] {
                    if let Some(value) = colors.get(key).filter(|value| !value.is_null()) {
                        *color = value
                            .as_str()
                            .and_then(parse_color)
                            .ok_or_else(invalid_color)?;
                    }
                }
            }
        }
        Ok(appearance)
    }
}

fn parse_color(value: &str) -> Option<Rgb> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8))
}

fn format_color(Rgb(red, green, blue): Rgb) -> String {
    format!("#{red:02X}{green:02X}{blue:02X}")
}

pub fn settings_path() -> Result<PathBuf, String> {
    let root = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| "LOCALAPPDATA was not found.".to_string())?;
    config_path(&PathBuf::from(root).join("QuotaTray"))
}

fn config_path(directory: &Path) -> Result<PathBuf, String> {
    let path = directory.join("config.json");
    let legacy = directory.join("sources.json");
    let migrate = || -> std::io::Result<()> {
        if !path.try_exists()? && legacy.try_exists()? {
            fs::rename(&legacy, &path)?;
        }
        Ok(())
    };
    migrate().map_err(|_| {
        "Cannot access config.json or migrate sources.json. Check the QuotaTray settings folder.".to_string()
    })?;
    Ok(path)
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
    fn appearance_errors_preserve_sources_and_cannot_overwrite_the_config() {
        for content in [
            r#"{"tray_theme":"lgiht","claude":{"source":"wsl","distro":"Ubuntu"},"codex":{"source":"windows"}}"#,
            r#"{"claude":{"source":"wsl","distro":"Ubuntu","colors":{"normal":"red"}},"codex":{"source":"windows"}}"#,
        ] {
            let mut settings = Settings::parse(content).unwrap();
            assert_eq!(settings.providers[0].source, Source::Wsl);
            assert_eq!(settings.providers[0].distro.as_deref(), Some("Ubuntu"));
            assert_eq!(settings.providers[1].source, Source::Windows);
            assert!(settings.appearance.is_err());
            settings.providers[0].source = Source::Windows;
            assert!(settings.to_json().is_err());
        }
    }

    #[test]
    fn appearance_errors_do_not_hide_invalid_sources() {
        assert!(
            Settings::parse(r#"{"tray_theme":"lgiht","claude":{"source":"windwos"}}"#).is_err()
        );
    }

    #[test]
    fn tray_theme_defaults_and_survives_saving_source_settings() {
        assert_eq!(
            Settings::parse("{}")
                .unwrap()
                .appearance
                .unwrap()
                .tray_theme,
            TrayTheme::Dark
        );
        let mut settings = Settings::parse(r#"{"tray_theme":"light"}"#).unwrap();
        settings.providers[0].source = Source::Wsl;
        let loaded = Settings::parse(&settings.to_json().unwrap().to_string()).unwrap();
        assert_eq!(loaded.appearance.unwrap().tray_theme, TrayTheme::Light);
        assert_eq!(loaded.providers[0].source, Source::Wsl);
        assert!(
            Settings::parse(r#"{"tray_theme":"lgiht"}"#)
                .unwrap()
                .appearance
                .is_err()
        );
        assert!(
            Settings::parse(r#"{"tray_theme":true}"#)
                .unwrap()
                .appearance
                .is_err()
        );
    }

    #[test]
    fn config_path_migrates_legacy_settings_without_overwriting_new_settings() {
        let directory = std::env::temp_dir().join(format!(
            "quota-tray-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("config.json");
        let legacy = directory.join("sources.json");
        assert_eq!(config_path(&directory).unwrap(), path);
        assert!(!path.exists());
        let original = r##"{"claude":{"source":"wsl","colors":{"normal":"#123456"}}}"##;
        fs::write(&legacy, original).unwrap();
        assert_eq!(config_path(&directory).unwrap(), path);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(!legacy.exists());
        let settings = Settings::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(settings.providers[0].source, Source::Wsl);
        assert_eq!(
            settings.appearance.as_ref().unwrap().colors[0].normal,
            Rgb(18, 52, 86)
        );
        fs::write(&legacy, "legacy file must not replace config").unwrap();
        assert_eq!(config_path(&directory).unwrap(), path);
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(legacy.exists());
        fs::write(&path, "invalid config must not fall back to legacy").unwrap();
        assert_eq!(config_path(&directory).unwrap(), path);
        assert!(Settings::parse(&fs::read_to_string(&path).unwrap()).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

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

    #[test]
    fn missing_colors_use_defaults_and_partial_overrides_keep_other_defaults() {
        let defaults = Appearance::default();
        assert_eq!(
            Settings::parse("{}").unwrap().appearance.unwrap().colors,
            defaults.colors
        );
        let settings = Settings::parse(
            r##"{"claude":{"colors":{"normal":"#12aB34"}},"codex":{"colors":{"warning":"#56789a"}}}"##,
        ).unwrap();
        assert_eq!(
            settings.appearance.as_ref().unwrap().colors[0].at(75.0),
            Rgb(18, 171, 52)
        );
        assert_eq!(
            settings.appearance.as_ref().unwrap().colors[0].at(80.0),
            defaults.colors[0].warning
        );
        assert_eq!(
            settings.appearance.as_ref().unwrap().colors[1].at(79.9),
            defaults.colors[1].normal
        );
        assert_eq!(
            settings.appearance.as_ref().unwrap().colors[1].at(80.0),
            Rgb(86, 120, 154)
        );
        let cleared = Settings::parse(
            r#"{"claude":{"colors":null},"codex":{"colors":{"normal":null,"warning":null}}}"#,
        )
        .unwrap();
        assert_eq!(cleared.appearance.as_ref().unwrap().colors, defaults.colors);
    }

    #[test]
    fn saving_a_source_change_preserves_color_overrides() {
        let mut settings =
            Settings::parse(r##"{"claude":{"colors":{"normal":"#123456","warning":"#ABCDEF"}}}"##)
                .unwrap();
        settings.providers[0].source = Source::Windows;
        let reloaded = Settings::parse(&settings.to_json().unwrap().to_string()).unwrap();
        assert_eq!(reloaded.providers, settings.providers);
        assert_eq!(
            reloaded.appearance.as_ref().unwrap().colors,
            settings.appearance.as_ref().unwrap().colors
        );
        assert_eq!(
            reloaded.appearance.as_ref().unwrap().colors[0].at(80.0),
            Rgb(171, 205, 239)
        );
    }

    #[test]
    fn malformed_colors_return_an_actionable_error() {
        for colors in [
            json!({"normal": "red"}),
            json!({"normal": "123456"}),
            json!({"normal": "#123"}),
            json!({"normal": "#GG1234"}),
            json!({"normal": "#é1234"}),
            json!({"warning": 123456}),
            json!({"warnng": "#123456"}),
            json!([]),
        ] {
            let content = json!({"claude": {"colors": colors}}).to_string();
            let error = Settings::parse(&content).unwrap().appearance.unwrap_err();
            assert!(error.contains("claude.colors"));
            assert!(error.contains("#RRGGBB"));
        }
    }
}
