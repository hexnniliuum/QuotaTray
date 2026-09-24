use crate::model::{Provider, ServiceStatusLevel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const BACKGROUND: Rgb = Rgb(28, 28, 30);
pub const TRACK: Rgb = Rgb(70, 70, 74);
pub const TEXT: Rgb = Rgb(255, 255, 255);
pub const MUTED_TEXT: Rgb = Rgb(166, 166, 172);

pub fn service_status_color(level: ServiceStatusLevel) -> Rgb {
    match level {
        ServiceStatusLevel::Operational => Rgb(34, 197, 94),
        ServiceStatusLevel::Degraded => Rgb(250, 204, 21),
        ServiceStatusLevel::PartialOutage => Rgb(249, 115, 22),
        ServiceStatusLevel::MajorOutage => Rgb(239, 68, 68),
        ServiceStatusLevel::Maintenance => Rgb(59, 130, 246),
        ServiceStatusLevel::Unavailable => MUTED_TEXT,
    }
}

pub fn usage_color(provider: Provider, used_percent: f64) -> Rgb {
    match (provider, used_percent) {
        (Provider::Claude, value) if value >= 90.0 => Rgb(159, 18, 57),
        (Provider::Claude, value) if value >= 70.0 => Rgb(249, 115, 22),
        (Provider::Claude, _) => Rgb(34, 197, 94),
        (Provider::Codex, value) if value >= 90.0 => Rgb(239, 68, 68),
        (Provider::Codex, value) if value >= 70.0 => Rgb(250, 204, 21),
        (Provider::Codex, _) => Rgb(59, 130, 246),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_palettes_remain_distinct_at_each_threshold() {
        for percentage in [20.0, 75.0, 95.0] {
            let colors = Provider::ALL.map(|provider| usage_color(provider, percentage));
            assert_ne!(colors[0], colors[1]);
        }
    }

    #[test]
    fn colors_follow_the_provider_specific_severity_order() {
        assert_eq!(usage_color(Provider::Claude, 20.0), Rgb(34, 197, 94));
        assert_eq!(usage_color(Provider::Claude, 75.0), Rgb(249, 115, 22));
        assert_eq!(usage_color(Provider::Claude, 95.0), Rgb(159, 18, 57));
        assert_eq!(usage_color(Provider::Codex, 20.0), Rgb(59, 130, 246));
        assert_eq!(usage_color(Provider::Codex, 75.0), Rgb(250, 204, 21));
        assert_eq!(usage_color(Provider::Codex, 95.0), Rgb(239, 68, 68));
    }

    #[test]
    fn service_status_colors_follow_severity() {
        assert_eq!(
            service_status_color(ServiceStatusLevel::Operational),
            Rgb(34, 197, 94)
        );
        assert_eq!(
            service_status_color(ServiceStatusLevel::MajorOutage),
            Rgb(239, 68, 68)
        );
        assert_eq!(
            service_status_color(ServiceStatusLevel::Unavailable),
            MUTED_TEXT
        );
    }
}
