use crate::model::{Provider, ServiceStatusLevel};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrayTheme {
    #[default]
    Dark,
    Light,
}

impl TrayTheme {
    pub fn dashboard(self) -> DashboardColors {
        match self {
            Self::Dark => DashboardColors {
                background: BACKGROUND,
                card: CARD,
                card_rule: CARD_RULE,
                pill: PILL,
                button: BUTTON,
                chip: CHIP,
                elapsed: ELAPSED,
                text: TEXT,
                soft_text: SOFT_TEXT,
                muted_text: MUTED_TEXT,
                dim_text: DIM_TEXT,
                off_text: OFF_TEXT,
                trace_head: TRACE_HEAD,
            },
            Self::Light => DashboardColors {
                // Tri Repetae cover base: #949576.
                background: Rgb(148, 149, 118),
                card: Rgb(177, 178, 150),
                card_rule: Rgb(133, 135, 104),
                pill: Rgb(163, 165, 135),
                button: Rgb(179, 181, 151),
                chip: Rgb(164, 166, 136),
                elapsed: Rgb(47, 50, 34),
                text: Rgb(25, 28, 19),
                soft_text: Rgb(37, 40, 27),
                muted_text: Rgb(40, 43, 30),
                dim_text: Rgb(43, 46, 32),
                off_text: Rgb(63, 66, 47),
                trace_head: Rgb(25, 28, 19),
            },
        }
    }

    pub fn dashboard_usage_color(self, color: Rgb) -> Rgb {
        if self == Self::Dark {
            return color;
        }
        let mut adjusted = self.usage_color(color);
        let colors = self.dashboard();
        while contrast(adjusted, blend(colors.pill, adjusted, 0.2)) < 3.0 {
            adjusted = blend(adjusted, Rgb(0, 0, 0), 0.08);
        }
        adjusted
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    pub fn background(self) -> Rgb {
        match self {
            Self::Dark => BACKGROUND,
            Self::Light => Rgb(245, 245, 247),
        }
    }

    pub fn text(self) -> Rgb {
        match self {
            Self::Dark => TEXT,
            Self::Light => Rgb(28, 28, 30),
        }
    }

    pub fn track(self) -> Rgb {
        match self {
            Self::Dark => TRACK,
            Self::Light => Rgb(205, 205, 210),
        }
    }

    pub fn usage_color(self, color: Rgb) -> Rgb {
        if self == Self::Dark {
            return color;
        }
        let mut adjusted = color;
        while contrast(adjusted, self.track()) < 3.0 {
            adjusted = blend(adjusted, Rgb(0, 0, 0), 0.08);
        }
        adjusted
    }
}

#[derive(Clone, Copy)]
pub struct DashboardColors {
    pub background: Rgb,
    pub card: Rgb,
    pub card_rule: Rgb,
    pub pill: Rgb,
    pub button: Rgb,
    pub chip: Rgb,
    pub elapsed: Rgb,
    pub text: Rgb,
    pub soft_text: Rgb,
    pub muted_text: Rgb,
    pub dim_text: Rgb,
    pub off_text: Rgb,
    pub trace_head: Rgb,
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let luminance = |Rgb(red, green, blue): Rgb| {
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageColors {
    pub normal: Rgb,
    pub warning: Rgb,
}

impl UsageColors {
    pub fn for_provider(provider: Provider) -> Self {
        match provider {
            Provider::Claude => Self {
                normal: Rgb(229, 184, 61),
                warning: Rgb(255, 69, 31),
            },
            Provider::Codex => Self {
                normal: Rgb(229, 231, 235),
                warning: Rgb(255, 50, 120),
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
    fn olive_dashboard_keeps_text_and_usage_readable() {
        let theme = TrayTheme::Light;
        let colors = theme.dashboard();
        for surface in [
            colors.background,
            colors.card,
            colors.pill,
            colors.button,
            colors.chip,
        ] {
            for text in [
                colors.text,
                colors.soft_text,
                colors.muted_text,
                colors.dim_text,
            ] {
                assert!(contrast(text, surface) >= 4.5);
            }
        }
        for percentage in [20.0, 80.0] {
            let adjusted = Provider::ALL.map(|provider| {
                let color = UsageColors::for_provider(provider).at(percentage);
                assert_eq!(TrayTheme::Dark.dashboard_usage_color(color), color);
                let color = theme.dashboard_usage_color(color);
                for surface in [colors.card, colors.pill] {
                    assert!(contrast(color, blend(surface, color, 0.2)) >= 3.0);
                }
                color
            });
            assert_ne!(adjusted[0], adjusted[1]);
        }
    }

    #[test]
    fn light_tray_colors_have_contrast_and_preserve_provider_distinction() {
        let theme = TrayTheme::Light;
        assert!(contrast(theme.text(), theme.background()) >= 7.0);
        for percentage in [20.0, 80.0] {
            let colors = Provider::ALL.map(|provider| {
                let color = UsageColors::for_provider(provider).at(percentage);
                assert_eq!(TrayTheme::Dark.usage_color(color), color);
                let light = theme.usage_color(color);
                assert!(contrast(light, theme.track()) >= 3.0);
                assert!(contrast(light, theme.background()) >= 3.0);
                light
            });
            assert_ne!(colors[0], colors[1]);
        }
        for color in [Rgb(0, 0, 0), Rgb(255, 255, 255), Rgb(255, 255, 0)] {
            assert!(contrast(theme.usage_color(color), theme.track()) >= 3.0);
        }
    }

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
                Rgb(229, 184, 61)
            );
            assert_eq!(
                UsageColors::for_provider(Provider::Codex).at(percentage),
                Rgb(229, 231, 235)
            );
        }
        for percentage in [80.0, 95.0, 100.0] {
            assert_eq!(
                UsageColors::for_provider(Provider::Claude).at(percentage),
                Rgb(255, 69, 31)
            );
            assert_eq!(
                UsageColors::for_provider(Provider::Codex).at(percentage),
                Rgb(255, 50, 120)
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
