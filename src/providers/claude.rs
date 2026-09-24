use std::fs;

use serde_json::Value;

use crate::model::{
    ExtraUsageBudget, Provider, ProviderSnapshot, UsageWindow, now_unix, parse_rfc3339_unix,
};
use crate::settings::{ProviderSettings, Source, try_sources};
use crate::{winhttp, wsl};

const USAGE_HOST: &str = "api.anthropic.com";
const USAGE_PATH: &str = "/api/oauth/usage";

pub fn refresh(config: &ProviderSettings) -> Result<ProviderSnapshot, String> {
    try_sources(config.source, |source| {
        let root = match source {
            Source::Wsl => wsl::config_path(config, Provider::Claude)?,
            _ => config.windows_dir(Provider::Claude)?,
        };
        let token = read_access_token_from(&root.join(".credentials.json"))?;
        fetch_usage(&token)
    })
}

fn fetch_usage(token: &str) -> Result<ProviderSnapshot, String> {
    let authorization = format!("Bearer {token}");
    let headers = [
        ("Authorization", authorization.as_str()),
        ("Accept", "application/json"),
        ("Content-Type", "application/json"),
        ("anthropic-beta", "oauth-2025-04-20"),
    ];
    let (status, body) = winhttp::get(USAGE_HOST, USAGE_PATH, &headers)?;
    if status != 200 {
        return Err(match status {
            401 => "Claude login expired. Sign in to Claude Code in the selected environment."
                .to_string(),
            403 => "Claude usage access is unavailable for this account.".to_string(),
            429 => "Claude usage service is rate-limiting refreshes.".to_string(),
            _ => format!("Claude usage service returned HTTP {status}."),
        });
    }

    let value: Value = serde_json::from_slice(&body)
        .map_err(|error| format!("Claude returned invalid usage data: {error}"))?;
    let mut snapshot = ProviderSnapshot::empty(Provider::Claude);
    snapshot.session = parse_window(&value, "five_hour", "Session")
        .or_else(|| parse_limit_window(&value, "session", "Session", None));
    snapshot.weekly = parse_window(&value, "seven_day", "Weekly")
        .or_else(|| parse_limit_window(&value, "weekly_all", "Weekly", None));
    if let Some(fable) = parse_limit_window(&value, "weekly_scoped", "Fable 5", Some("fable"))
        .or_else(|| parse_window(&value, "seven_day_overage_included", "Fable 5"))
    {
        snapshot.model_windows.push(fable);
    }
    snapshot.extra_usage = parse_extra_usage(&value);
    snapshot.last_updated_unix = Some(now_unix());
    if snapshot.session.is_none()
        && snapshot.weekly.is_none()
        && snapshot.model_windows.is_empty()
        && snapshot.extra_usage.is_none()
    {
        return Err("Claude returned no session or weekly usage windows.".to_string());
    }
    Ok(snapshot)
}

fn read_access_token_from(path: &std::path::Path) -> Result<String, String> {
    let content = fs::read_to_string(path).map_err(|_| {
        "Claude login was not found. Sign in to Claude Code or check Sources.".to_string()
    })?;
    let value: Value = serde_json::from_str(&content)
        .map_err(|_| "Claude credentials could not be read.".to_string())?;
    value
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Claude OAuth login was not found.".to_string())
}

fn parse_window(root: &Value, key: &str, label: &str) -> Option<UsageWindow> {
    let value = root.get(key)?;
    let utilization = value.get("utilization")?.as_f64()?;
    let resets_at = value
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_unix);
    Some(UsageWindow::new(label, utilization, resets_at))
}

fn parse_limit_window(
    root: &Value,
    kind: &str,
    label: &str,
    model_name: Option<&str>,
) -> Option<UsageWindow> {
    let limit = root.get("limits")?.as_array()?.iter().find(|limit| {
        if limit.get("kind").and_then(Value::as_str) != Some(kind) {
            return false;
        }
        let Some(model_name) = model_name else {
            return true;
        };
        limit
            .pointer("/scope/model/display_name")
            .and_then(Value::as_str)
            .is_some_and(|display_name| {
                display_name
                    .to_ascii_lowercase()
                    .contains(&model_name.to_ascii_lowercase())
            })
    })?;
    let used_percent = limit.get("percent")?.as_f64()?;
    let resets_at = limit
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_unix);
    if used_percent == 0.0 && resets_at.is_none() {
        return None;
    }
    Some(UsageWindow::new(label, used_percent, resets_at))
}

fn parse_extra_usage(root: &Value) -> Option<ExtraUsageBudget> {
    let extra = root.get("extra_usage")?;
    if !extra.get("is_enabled")?.as_bool()? {
        return None;
    }

    if let Some(used) = root.pointer("/spend/used") {
        let used_minor = used.get("amount_minor")?.as_i64()?;
        let currency = used.get("currency")?.as_str()?;
        let decimal_places = used.get("exponent")?.as_u64()?.try_into().ok()?;
        let limit_minor = root
            .pointer("/spend/limit")
            .filter(|limit| !limit.is_null())
            .and_then(|limit| {
                let same_units = limit.get("currency")?.as_str()? == currency
                    && limit.get("exponent")?.as_u64()? == u64::from(decimal_places);
                same_units
                    .then(|| limit.get("amount_minor")?.as_i64())
                    .flatten()
            });
        return Some(ExtraUsageBudget::new(
            used_minor,
            limit_minor,
            currency,
            decimal_places,
        ));
    }

    let used_minor = extra.get("used_credits")?.as_f64()?.round() as i64;
    let limit_minor = extra
        .get("monthly_limit")
        .filter(|limit| !limit.is_null())
        .and_then(Value::as_f64)
        .map(|limit| limit.round() as i64);
    let currency = extra.get("currency")?.as_str()?;
    let decimal_places = extra.get("decimal_places")?.as_u64()?.try_into().ok()?;
    Some(ExtraUsageBudget::new(
        used_minor,
        limit_minor,
        currency,
        decimal_places,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_extra_usage_spend_and_cap() {
        let value = serde_json::json!({
            "extra_usage": {
                "is_enabled": true,
                "monthly_limit": 10000,
                "used_credits": 7426.0,
                "currency": "EUR",
                "decimal_places": 2
            },
            "spend": {
                "used": {
                    "amount_minor": 7426,
                    "currency": "EUR",
                    "exponent": 2
                },
                "limit": {
                    "amount_minor": 10000,
                    "currency": "EUR",
                    "exponent": 2
                }
            }
        });

        let budget = parse_extra_usage(&value).unwrap();
        assert_eq!(budget.used_minor, 7_426);
        assert_eq!(budget.limit_minor, Some(10_000));
        assert_eq!(budget.remaining_minor(), Some(2_574));
    }

    #[test]
    fn ignores_disabled_extra_usage() {
        let value = serde_json::json!({
            "extra_usage": {
                "is_enabled": false
            }
        });

        assert!(parse_extra_usage(&value).is_none());
    }

    #[test]
    fn parses_fable_usage_from_scoped_weekly_limit() {
        let value = serde_json::json!({
            "limits": [
                {
                    "kind": "weekly_scoped",
                    "percent": 44.0,
                    "resets_at": "2026-07-17T22:59:59+00:00",
                    "scope": {
                        "model": {
                            "id": null,
                            "display_name": "Fable"
                        }
                    }
                }
            ]
        });

        let window = parse_limit_window(&value, "weekly_scoped", "Fable 5", Some("fable")).unwrap();
        assert_eq!(window.label, "Fable 5");
        assert_eq!(window.used_percent, 44.0);
        assert_eq!(window.resets_at_unix, Some(1_784_329_199));
    }

    #[test]
    fn ignores_non_fable_scoped_weekly_limit() {
        let value = serde_json::json!({
            "limits": [
                {
                    "kind": "weekly_scoped",
                    "percent": 31.0,
                    "resets_at": "2026-07-17T22:59:59+00:00",
                    "scope": {"model": {"display_name": "Opus"}}
                }
            ]
        });

        assert!(parse_limit_window(&value, "weekly_scoped", "Fable 5", Some("fable")).is_none());
    }

    #[test]
    fn parses_session_and_weekly_from_limits_array() {
        let value = serde_json::json!({
            "limits": [
                {
                    "kind": "session",
                    "percent": 12.0,
                    "resets_at": "2026-07-17T22:59:59+00:00"
                },
                {
                    "kind": "weekly_all",
                    "percent": 27.0,
                    "resets_at": "2026-07-20T22:59:59+00:00"
                }
            ]
        });

        let session = parse_limit_window(&value, "session", "Session", None).unwrap();
        let weekly = parse_limit_window(&value, "weekly_all", "Weekly", None).unwrap();
        assert_eq!(session.used_percent, 12.0);
        assert_eq!(weekly.used_percent, 27.0);
    }
}
