use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Self; 2] = [Self::Claude, Self::Codex];
    pub const COUNT: usize = Self::ALL.len();

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude",
            Self::Codex => "Codex",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub const fn index(self) -> usize {
        match self {
            Self::Claude => 0,
            Self::Codex => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceStatusLevel {
    Operational,
    Degraded,
    PartialOutage,
    MajorOutage,
    Maintenance,
    Unavailable,
}

impl ServiceStatusLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Operational => "Operational",
            Self::Degraded => "Degraded",
            Self::PartialOutage => "Partial outage",
            Self::MajorOutage => "Major outage",
            Self::Maintenance => "Maintenance",
            Self::Unavailable => "Status unavailable",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceStatus {
    pub level: ServiceStatusLevel,
    pub error: Option<String>,
}

impl ServiceStatus {
    pub fn new(level: ServiceStatusLevel) -> Self {
        Self { level, error: None }
    }

    pub fn from_error(error: String) -> Self {
        Self {
            level: ServiceStatusLevel::Unavailable,
            error: Some(error),
        }
    }

    pub fn label(&self) -> &'static str {
        self.level.label()
    }
}

#[derive(Clone, Debug)]
pub struct UsageWindow {
    pub label: String,
    pub used_percent: f64,
    pub resets_at_unix: Option<i64>,
    pub duration_secs: Option<i64>,
}

impl UsageWindow {
    pub fn new(label: impl Into<String>, used_percent: f64, resets_at_unix: Option<i64>) -> Self {
        Self {
            label: label.into(),
            used_percent: used_percent.clamp(0.0, 100.0),
            resets_at_unix,
            duration_secs: None,
        }
    }

    pub fn with_duration_secs(mut self, duration_secs: Option<i64>) -> Self {
        self.duration_secs = duration_secs.filter(|secs| *secs > 0);
        self
    }

    pub fn is_applicable(&self, now_unix: i64) -> bool {
        self.resets_at_unix.is_none_or(|reset| reset > now_unix)
    }

    /// How much of the window has passed, when both its reset time and
    /// length are known.
    pub fn elapsed_percent(&self, now_unix: i64) -> Option<f64> {
        let reset = self.resets_at_unix?;
        let duration = self.duration_secs.filter(|secs| *secs > 0)?;
        let elapsed = now_unix - (reset - duration);
        Some((elapsed as f64 / duration as f64 * 100.0).clamp(0.0, 100.0))
    }
}

#[derive(Clone, Debug)]
pub struct ExtraUsageBudget {
    pub used_minor: i64,
    pub limit_minor: Option<i64>,
    pub currency: String,
    pub decimal_places: u32,
}

impl ExtraUsageBudget {
    pub fn new(
        used_minor: i64,
        limit_minor: Option<i64>,
        currency: impl Into<String>,
        decimal_places: u32,
    ) -> Self {
        Self {
            used_minor: used_minor.max(0),
            limit_minor: limit_minor.map(|limit| limit.max(0)),
            currency: currency.into(),
            decimal_places: decimal_places.min(9),
        }
    }

    pub fn remaining_minor(&self) -> Option<i64> {
        self.limit_minor
            .map(|limit| limit.saturating_sub(self.used_minor).max(0))
    }

    pub fn format_amount(&self, amount_minor: i64) -> String {
        format_money(amount_minor, self.currency.as_str(), self.decimal_places)
    }
}

#[derive(Clone, Debug)]
pub struct ResetCredit {
    pub id: String,
    pub title: String,
    pub description: String,
    pub expires_at: Option<i64>,
    pub expiry_known: bool,
}

#[derive(Clone, Debug)]
pub struct ResetCredits {
    pub available_count: u64,
    /// None means the service supplied only a count. Rows may be capped.
    pub credits: Option<Vec<ResetCredit>>,
}

impl ResetCredits {
    pub fn choices(&self, now: i64) -> Vec<ResetCredit> {
        let mut credits: Vec<_> = self
            .credits
            .iter()
            .flatten()
            .filter(|credit| credit.expires_at.is_none_or(|expiry| expiry > now))
            .cloned()
            .collect();
        credits.sort_by_key(|credit| credit.expires_at.unwrap_or(i64::MAX));
        credits
    }
}

#[derive(Clone, Debug)]
pub struct ProviderSnapshot {
    pub provider: Provider,
    pub session: Option<UsageWindow>,
    pub weekly: Option<UsageWindow>,
    pub model_windows: Vec<UsageWindow>,
    pub extra_usage: Option<ExtraUsageBudget>,
    pub reset_credits: Option<ResetCredits>,
    pub last_updated_unix: Option<i64>,
    pub from_session_history: bool,
    pub error: Option<String>,
}

impl ProviderSnapshot {
    pub fn empty(provider: Provider) -> Self {
        Self {
            provider,
            session: None,
            weekly: None,
            model_windows: Vec::new(),
            extra_usage: None,
            reset_credits: None,
            last_updated_unix: None,
            from_session_history: false,
            error: None,
        }
    }

    pub fn displayed_window(&self, now_unix: i64) -> Option<&UsageWindow> {
        self.session
            .as_ref()
            .filter(|window| window.is_applicable(now_unix))
            .or_else(|| {
                self.weekly
                    .as_ref()
                    .filter(|window| window.is_applicable(now_unix))
            })
    }

    pub fn record_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub fn freshness_label(&self, now_unix: i64) -> String {
        match (self.from_session_history, self.last_updated_unix) {
            (true, Some(updated)) => {
                format!(
                    "Saved session usage · recorded {}",
                    age_label(updated, now_unix)
                )
            }
            (true, None) => "Saved session usage · recording time unknown".to_string(),
            (false, Some(updated)) => format!("Updated {}", age_label(updated, now_unix)),
            (false, None) => "Not updated yet".to_string(),
        }
    }
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn parse_rfc3339_unix(value: &str) -> Option<i64> {
    if value.len() < 19 {
        return None;
    }
    let year = value.get(0..4)?.parse::<i64>().ok()?;
    let month = value.get(5..7)?.parse::<i64>().ok()?;
    let day = value.get(8..10)?.parse::<i64>().ok()?;
    let hour = value.get(11..13)?.parse::<i64>().ok()?;
    let minute = value.get(14..16)?.parse::<i64>().ok()?;
    let second = value.get(17..19)?.parse::<i64>().ok()?;
    let mut timestamp =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;

    if let Some(position) = value.get(19..)?.find(['+', '-']) {
        let offset_start = 19 + position;
        let sign = if value.as_bytes().get(offset_start) == Some(&b'+') {
            1
        } else {
            -1
        };
        let offset_hour = value
            .get(offset_start + 1..offset_start + 3)?
            .parse::<i64>()
            .ok()?;
        let offset_minute = value
            .get(offset_start + 4..offset_start + 6)?
            .parse::<i64>()
            .ok()?;
        timestamp -= sign * (offset_hour * 3_600 + offset_minute * 60);
    }
    Some(timestamp)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn format_countdown(reset_unix: Option<i64>, now_unix: i64) -> String {
    let Some(reset) = reset_unix else {
        return "reset unknown".to_string();
    };
    let remaining = reset.saturating_sub(now_unix);
    if remaining <= 0 {
        return "reset due".to_string();
    }
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3_600;
    let minutes = (remaining % 3_600) / 60;
    if days > 0 {
        format!("resets in {days}d {hours}h")
    } else if hours > 0 {
        format!("resets in {hours}h {minutes}m")
    } else {
        format!("resets in {}m", minutes.max(1))
    }
}

fn age_label(updated_unix: i64, now_unix: i64) -> String {
    let seconds = now_unix.saturating_sub(updated_unix);
    if seconds < 60 {
        "just now".to_string()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / 3_600)
    }
}

fn format_money(amount_minor: i64, currency: &str, decimal_places: u32) -> String {
    let divisor = 10_i64.pow(decimal_places);
    let amount = amount_minor.max(0);
    let major = amount / divisor;
    let fraction = amount % divisor;
    let number = if decimal_places == 0 {
        major.to_string()
    } else {
        format!(
            "{major}.{fraction:0width$}",
            width = decimal_places as usize
        )
    };
    match currency {
        "EUR" => format!("€{number}"),
        "GBP" => format!("£{number}"),
        "JPY" => format!("¥{number}"),
        "USD" => format!("${number}"),
        _ => format!("{number} {currency}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_handles_before_at_and_after_reset() {
        assert_eq!(format_countdown(Some(1_000), 999), "resets in 1m");
        assert_eq!(format_countdown(Some(1_000), 1_000), "reset due");
        assert_eq!(format_countdown(Some(1_000), 1_001), "reset due");
        assert_eq!(format_countdown(None, 1_001), "reset unknown");
    }

    #[test]
    fn session_is_preferred_even_when_weekly_is_higher() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Claude);
        snapshot.session = Some(UsageWindow::new("Session", 12.0, Some(2_000)));
        snapshot.weekly = Some(UsageWindow::new("Weekly", 92.0, Some(9_000)));

        assert_eq!(snapshot.displayed_window(1_000).unwrap().used_percent, 12.0);
    }

    #[test]
    fn weekly_is_used_when_session_is_expired() {
        let mut snapshot = ProviderSnapshot::empty(Provider::Codex);
        snapshot.session = Some(UsageWindow::new("Session", 80.0, Some(999)));
        snapshot.weekly = Some(UsageWindow::new("Weekly", 41.0, Some(9_000)));

        assert_eq!(snapshot.displayed_window(1_000).unwrap().used_percent, 41.0);
    }

    #[test]
    fn elapsed_percent_needs_both_reset_and_duration() {
        let window =
            UsageWindow::new("Session", 10.0, Some(4_000)).with_duration_secs(Some(10_000));
        assert_eq!(window.elapsed_percent(1_000), Some(70.0));
        assert_eq!(window.elapsed_percent(-9_000), Some(0.0));
        assert_eq!(window.elapsed_percent(9_000), Some(100.0));
        assert_eq!(
            UsageWindow::new("Session", 10.0, Some(4_000)).elapsed_percent(1_000),
            None
        );
        assert_eq!(
            UsageWindow::new("Session", 10.0, None)
                .with_duration_secs(Some(10))
                .elapsed_percent(1_000),
            None
        );
        assert_eq!(
            UsageWindow::new("Session", 10.0, Some(4_000))
                .with_duration_secs(Some(0))
                .duration_secs,
            None
        );
    }

    #[test]
    fn extra_usage_calculates_and_formats_remaining_budget() {
        let budget = ExtraUsageBudget::new(7_426, Some(10_000), "EUR", 2);

        assert_eq!(budget.remaining_minor(), Some(2_574));
        assert_eq!(budget.format_amount(2_574), "€25.74");
    }

    #[test]
    fn extra_usage_never_reports_a_negative_remaining_budget() {
        let budget = ExtraUsageBudget::new(12_000, Some(10_000), "USD", 2);

        assert_eq!(budget.remaining_minor(), Some(0));
    }

    #[test]
    fn parses_utc_and_offset_timestamps() {
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_unix("1970-01-01T01:00:00+01:00"), Some(0));
    }
}
