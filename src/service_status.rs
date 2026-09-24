use serde_json::Value;

use crate::model::{Provider, ServiceStatus, ServiceStatusLevel};
use crate::winhttp;

pub fn refresh(provider: Provider) -> Result<ServiceStatus, String> {
    let host = match provider {
        Provider::Claude => "status.claude.com",
        Provider::Codex => "status.openai.com",
    };
    let (status, body) = winhttp::get(host, "/api/v2/status.json", &[])?;
    if status != 200 {
        return Err(format!(
            "{} status service returned HTTP {status}.",
            provider.name()
        ));
    }
    parse_status(&body)
}

fn parse_status(body: &[u8]) -> Result<ServiceStatus, String> {
    let root: Value = serde_json::from_slice(body)
        .map_err(|error| format!("Invalid status response: {error}"))?;
    let status = root
        .get("status")
        .ok_or_else(|| "Status response did not contain a status.".to_string())?;
    let indicator = status
        .get("indicator")
        .and_then(Value::as_str)
        .ok_or_else(|| "Status response did not contain an indicator.".to_string())?;
    let description = status
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let normalized = description.to_ascii_lowercase();
    let level = if normalized.contains("maintenance") {
        ServiceStatusLevel::Maintenance
    } else {
        match indicator {
            "none" => ServiceStatusLevel::Operational,
            "minor" => ServiceStatusLevel::Degraded,
            "major" => ServiceStatusLevel::PartialOutage,
            "critical" => ServiceStatusLevel::MajorOutage,
            _ => return Err("Unknown status indicator.".into()),
        }
    };
    Ok(ServiceStatus::new(level))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operational_status() {
        let status = parse_status(
            br#"{"status":{"description":"All Systems Operational","indicator":"none"}}"#,
        )
        .unwrap();

        assert_eq!(status.level, ServiceStatusLevel::Operational);
        assert_eq!(status.label(), "Operational");
    }

    #[test]
    fn maps_statuspage_major_indicator_to_partial_outage() {
        let status = parse_status(
            br#"{"status":{"description":"Partial System Outage","indicator":"major"}}"#,
        )
        .unwrap();

        assert_eq!(status.level, ServiceStatusLevel::PartialOutage);
        assert_eq!(status.label(), "Partial outage");
    }

    #[test]
    fn maps_statuspage_critical_indicator_to_major_outage() {
        let status = parse_status(
            br#"{"status":{"description":"Major Service Outage","indicator":"critical"}}"#,
        )
        .unwrap();

        assert_eq!(status.level, ServiceStatusLevel::MajorOutage);
        assert_eq!(status.label(), "Major outage");
    }

    #[test]
    fn recognizes_maintenance_description() {
        let status = parse_status(
            br#"{"status":{"description":"Service Under Maintenance","indicator":"minor"}}"#,
        )
        .unwrap();

        assert_eq!(status.level, ServiceStatusLevel::Maintenance);
        assert_eq!(status.label(), "Maintenance");
    }

    #[test]
    fn rejects_unknown_indicators_instead_of_implying_health() {
        let error = parse_status(br#"{"status":{"description":"Mystery","indicator":"other"}}"#)
            .unwrap_err();

        assert!(error.contains("Unknown status indicator"));
    }
}
