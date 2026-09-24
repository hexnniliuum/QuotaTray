//! Refresh trace: a monitor-style line that sweeps along a usage bar and
//! spikes once where the fill ends. Pure geometry; the UI draws the result.

/// Pixels the head travels per millisecond (layout pixels, before DPI scaling).
pub const SPEED: f64 = 0.55;
/// Length of the bright head segment.
pub const COMET: f64 = 56.0;
/// How long the dim trail lingers after the head leaves the bar.
pub const FADE_MS: f64 = 320.0;
/// Delay between one bar starting its trace and the next.
pub const STAGGER_MS: f64 = 110.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Shape {
    Spike,
    Qrs,
    Double,
    Notch,
}

impl Shape {
    const ALL: [Self; 4] = [Self::Spike, Self::Qrs, Self::Double, Self::Notch];

    fn width(self) -> f64 {
        match self {
            Self::Spike => 16.0,
            Self::Qrs => 15.0,
            Self::Double => 17.0,
            Self::Notch => 20.0,
        }
    }

    /// Relative segments `(dx, dy)` with `amplitude` as the peak height.
    fn segments(self, a: f64) -> Vec<(f64, f64)> {
        match self {
            Self::Spike => vec![
                (4.0, 0.0),
                (3.0, -a),
                (3.0, a * 1.25),
                (3.0, -a * 0.25),
                (3.0, 0.0),
            ],
            Self::Qrs => vec![
                (3.0, a * 0.15),
                (3.0, -a * 1.15),
                (3.0, a * 1.35),
                (3.0, -a * 0.35),
                (3.0, 0.0),
            ],
            Self::Double => vec![
                (3.0, -a * 0.55),
                (3.0, a * 0.55),
                (2.0, 0.0),
                (3.0, -a),
                (3.0, a),
                (3.0, 0.0),
            ],
            Self::Notch => vec![
                (1.5, -a * 0.15),
                (3.0, 0.0),
                (1.5, a * 0.15),
                (2.0, 0.0),
                (3.0, -a),
                (3.0, a * 1.15),
                (3.0, -a * 0.15),
                (3.0, 0.0),
            ],
        }
    }
}

/// Deterministic value in `[0, 1)` for a bar of a given pulse.
pub fn random(seed: u64, index: usize, salt: u64) -> f64 {
    let mut z = seed
        .wrapping_add((index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(salt.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// The full polyline for one bar, in layout pixels relative to the bar's left
/// edge, with `y` measured from the bar's vertical centre (negative is up).
#[derive(Clone, Debug)]
pub struct Trace {
    points: Vec<(f64, f64)>,
    cumulative: Vec<f64>,
    pub total: f64,
}

/// Which spike a bar gets for this pulse.
pub fn pick_shape(seed: u64, index: usize) -> Shape {
    Shape::ALL[(random(seed, index, 1) * Shape::ALL.len() as f64) as usize]
}

impl Trace {
    pub fn build(width: f64, fill_end: f64, seed: u64, index: usize) -> Self {
        let amplitude = 13.0 + random(seed, index, 2) * 8.0;
        Self::with_shape(width, fill_end, pick_shape(seed, index), amplitude)
    }

    pub fn with_shape(width: f64, fill_end: f64, shape: Shape, amplitude: f64) -> Self {
        let mut points = vec![(0.0, 0.0)];
        if fill_end > 0.0 && width > shape.width() {
            let x0 = (fill_end - shape.width() / 2.0).clamp(0.0, width - shape.width());
            let (mut x, mut y) = (x0, 0.0);
            points.push((x, y));
            for (dx, dy) in shape.segments(amplitude) {
                x += dx;
                y += dy;
                points.push((x, y));
            }
        }
        points.push((width, 0.0));
        let mut cumulative = Vec::with_capacity(points.len());
        let mut total = 0.0;
        cumulative.push(0.0);
        for pair in points.windows(2) {
            let (ax, ay) = pair[0];
            let (bx, by) = pair[1];
            total += ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
            cumulative.push(total);
        }
        Self {
            points,
            cumulative,
            total,
        }
    }

    /// Milliseconds for the head to sweep the whole bar and leave it.
    pub fn duration_ms(&self) -> f64 {
        (self.total + COMET) / SPEED
    }

    /// The part of the trace between two distances along it.
    pub fn segment(&self, from: f64, to: f64) -> Vec<(f64, f64)> {
        let from = from.max(0.0);
        let to = to.min(self.total);
        if to <= from {
            return Vec::new();
        }
        let mut out = vec![self.at(from)];
        for (index, &length) in self.cumulative.iter().enumerate() {
            if length > from && length < to {
                out.push(self.points[index]);
            }
        }
        out.push(self.at(to));
        out
    }

    fn at(&self, distance: f64) -> (f64, f64) {
        let index = self
            .cumulative
            .iter()
            .position(|&length| length >= distance)
            .unwrap_or(self.cumulative.len() - 1)
            .max(1);
        let (ax, ay) = self.points[index - 1];
        let (bx, by) = self.points[index];
        let span = self.cumulative[index] - self.cumulative[index - 1];
        let k = if span > 0.0 {
            ((distance - self.cumulative[index - 1]) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (ax + (bx - ax) * k, ay + (by - ay) * k)
    }
}

/// What to draw for one bar at a moment in time.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub trail: Vec<(f64, f64)>,
    pub head: Vec<(f64, f64)>,
    /// 1.0 while the sweep runs, falling to 0.0 as the trail fades.
    pub trail_opacity: f64,
}

impl Frame {
    /// `local_ms` is the time since this bar's own sweep started; `None` when
    /// the bar has nothing to draw (not started yet, or finished).
    pub fn at(trace: &Trace, local_ms: f64) -> Option<Self> {
        let duration = trace.duration_ms();
        if local_ms < 0.0 || local_ms > duration + FADE_MS {
            return None;
        }
        let head_distance = local_ms.min(duration) * SPEED;
        let trail_opacity = if local_ms > duration {
            1.0 - (local_ms - duration) / FADE_MS
        } else {
            1.0
        };
        Some(Self {
            trail: trace.segment(0.0, head_distance),
            head: trace.segment(head_distance - COMET, head_distance),
            trail_opacity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spike_sits_where_the_fill_ends() {
        let trace = Trace::with_shape(300.0, 120.0, Shape::Spike, 16.0);
        let peak = trace
            .points
            .iter()
            .copied()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .unwrap();
        assert!((peak.0 - 120.0).abs() <= Shape::Spike.width() / 2.0);
        assert_eq!(peak.1, -16.0);
        assert_eq!(trace.points.first(), Some(&(0.0, 0.0)));
        assert_eq!(trace.points.last(), Some(&(300.0, 0.0)));
    }

    #[test]
    fn empty_bar_draws_a_flat_line() {
        let trace = Trace::with_shape(300.0, 0.0, Shape::Qrs, 16.0);
        assert_eq!(trace.points.len(), 2);
        assert_eq!(trace.total, 300.0);
    }

    #[test]
    fn full_bar_keeps_the_spike_inside_the_track() {
        let trace = Trace::with_shape(300.0, 300.0, Shape::Notch, 16.0);
        assert!(trace.points.iter().all(|point| point.0 <= 300.0));
    }

    #[test]
    fn segment_interpolates_both_ends() {
        let trace = Trace::with_shape(100.0, 0.0, Shape::Spike, 10.0);
        assert_eq!(trace.segment(10.0, 40.0), vec![(10.0, 0.0), (40.0, 0.0)]);
        assert!(trace.segment(50.0, 50.0).is_empty());
        assert!(trace.segment(120.0, 150.0).is_empty());
    }

    #[test]
    fn frame_runs_then_fades_then_stops() {
        let trace = Trace::with_shape(100.0, 50.0, Shape::Spike, 10.0);
        assert!(Frame::at(&trace, -1.0).is_none());
        let running = Frame::at(&trace, 20.0).unwrap();
        assert_eq!(running.trail_opacity, 1.0);
        assert!(!running.head.is_empty());
        let fading = Frame::at(&trace, trace.duration_ms() + FADE_MS / 2.0).unwrap();
        assert!((fading.trail_opacity - 0.5).abs() < 1e-9);
        assert!(fading.head.is_empty());
        assert!(Frame::at(&trace, trace.duration_ms() + FADE_MS + 1.0).is_none());
    }

    #[test]
    fn randomness_is_deterministic_per_seed_and_bar() {
        assert_eq!(random(7, 0, 1), random(7, 0, 1));
        assert_ne!(random(7, 0, 1), random(7, 1, 1));
        assert_ne!(random(7, 0, 1), random(8, 0, 1));
        assert!((0.0..1.0).contains(&random(7, 3, 2)));
        let shapes: std::collections::HashSet<_> =
            (0..64).map(|index| pick_shape(99, index)).collect();
        assert_eq!(shapes.len(), 4);
    }
}
