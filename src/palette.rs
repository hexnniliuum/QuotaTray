use crate::model::{Provider, ServiceStatusLevel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageColors {
    pub normal: Rgb,
    pub warning: Rgb,
}

impl UsageColors {
    pub fn for_provider(provider: Provider) -> Self {
        match provider {
            Provider::Claude => Self {
                normal: Rgb(222, 134, 89),
                warning: Rgb(242, 100, 64),
            },
            Provider::Codex => Self {
                normal: Rgb(229, 231, 235),
                warning: Rgb(241, 107, 141),
            },
        }
    }

    pub fn at(self, used_percent: f64) -> Rgb {
        if used_percent >= 80.0 {
            self.warning
        } else {
            self.normal
        }
    }
}

pub const BACKGROUND: Rgb = Rgb(28, 28, 30);
pub const CARD: Rgb = Rgb(36, 36, 38);
pub const CARD_RULE: Rgb = Rgb(49, 49, 51);
pub const PILL: Rgb = Rgb(49, 49, 51);
pub const BUTTON: Rgb = Rgb(46, 46, 48);
pub const CHIP: Rgb = Rgb(42, 42, 44);
pub const TRACK: Rgb = Rgb(70, 70, 74);
pub const ELAPSED: Rgb = Rgb(154, 154, 163);
pub const TEXT: Rgb = Rgb(255, 255, 255);
pub const SOFT_TEXT: Rgb = Rgb(208, 208, 213);
pub const MUTED_TEXT: Rgb = Rgb(166, 166, 172);
pub const DIM_TEXT: Rgb = Rgb(142, 142, 147);
pub const OFF_TEXT: Rgb = Rgb(110, 110, 115);
pub const TRACE_HEAD: Rgb = Rgb(255, 255, 255);

/// Mixes `amount` of `foreground` into `background`.
pub fn blend(background: Rgb, foreground: Rgb, amount: f64) -> Rgb {
    let amount = amount.clamp(0.0, 1.0);
    let channel = |back: u8, front: u8| {
        (f64::from(back) + (f64::from(front) - f64::from(back)) * amount).round() as u8
    };
    Rgb(
        channel(background.0, foreground.0),
        channel(background.1, foreground.1),
        channel(background.2, foreground.2),
    )
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_palettes_remain_distinct_at_each_threshold() {
        for percentage in [20.0, 75.0, 79.9, 80.0, 95.0] {
            let colors =
                Provider::ALL.map(|provider| UsageColors::for_provider(provider).at(percentage));
            assert_ne!(colors[0], colors[1]);
        }
    }

    #[test]
    fn colors_follow_the_provider_specific_severity_order() {
        for percentage in [20.0, 75.0, 79.9] {
            assert_eq!(
                UsageColors::for_provider(Provider::Claude).at(percentage),
                Rgb(222, 134, 89)
            );
            assert_eq!(
                UsageColors::for_provider(Provider::Codex).at(percentage),
                Rgb(229, 231, 235)
            );
        }
        for percentage in [80.0, 95.0, 100.0] {
            assert_eq!(
                UsageColors::for_provider(Provider::Claude).at(percentage),
                Rgb(242, 100, 64)
            );
            assert_eq!(
                UsageColors::for_provider(Provider::Codex).at(percentage),
                Rgb(241, 107, 141)
            );
        }
    }

    #[test]
    fn blend_moves_between_the_two_colors() {
        assert_eq!(blend(Rgb(0, 0, 0), Rgb(255, 255, 255), 0.0), Rgb(0, 0, 0));
        assert_eq!(
            blend(Rgb(0, 0, 0), Rgb(255, 255, 255), 1.0),
            Rgb(255, 255, 255)
        );
        assert_eq!(
            blend(Rgb(0, 100, 200), Rgb(100, 100, 0), 0.5),
            Rgb(50, 100, 100)
        );
        assert_eq!(
            blend(Rgb(0, 0, 0), Rgb(255, 255, 255), 2.0),
            Rgb(255, 255, 255)
        );
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
