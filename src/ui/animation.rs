use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, KillTimer, SetTimer};

pub(super) const PULSE_TIMER: usize = 1;
const PULSE_FRAME_MS: u32 = 16;

#[derive(Clone, Copy)]
pub(super) struct PulseClock {
    pub elapsed_ms: f64,
    pub seed: u64,
}

struct Pulse {
    started: Instant,
    seed: u64,
}

pub(super) struct DashboardAnimation {
    generation: u64,
    pulse: Option<Pulse>,
}

impl DashboardAnimation {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            pulse: None,
        }
    }

    pub fn on_refresh(&mut self, hwnd: HWND, generation: u64) {
        if self.observe_refresh(generation, unsafe { IsWindowVisible(hwnd).as_bool() }) {
            unsafe {
                SetTimer(Some(hwnd), PULSE_TIMER, PULSE_FRAME_MS, None);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
    }

    fn observe_refresh(&mut self, generation: u64, visible: bool) -> bool {
        if self.generation == generation {
            return false;
        }
        self.generation = generation;
        if !visible {
            return false;
        }
        self.pulse = Some(Pulse {
            started: Instant::now(),
            seed: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(1),
        });
        true
    }

    pub fn frame(&self) -> Option<PulseClock> {
        self.pulse.as_ref().map(|pulse| PulseClock {
            elapsed_ms: pulse.started.elapsed().as_secs_f64() * 1_000.0,
            seed: pulse.seed,
        })
    }

    pub fn finish_frame(&mut self, hwnd: HWND, animating: bool) {
        if !animating && self.pulse.is_some() {
            self.stop(hwnd);
        }
    }

    pub fn stop(&mut self, hwnd: HWND) {
        self.pulse = None;
        unsafe {
            let _ = KillTimer(Some(hwnd), PULSE_TIMER);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_refresh_starts_once_when_its_notification_is_consumed() {
        let mut animation = DashboardAnimation::new(0);
        assert!(!animation.observe_refresh(0, true));
        assert!(animation.observe_refresh(1, true));
        let seed = animation.frame().unwrap().seed;
        assert!(!animation.observe_refresh(1, true));
        assert_eq!(animation.frame().unwrap().seed, seed);
        assert!(animation.observe_refresh(3, true));
    }

    #[test]
    fn hidden_dashboard_does_not_replay_old_refreshes() {
        let mut animation = DashboardAnimation::new(4);
        assert!(!animation.observe_refresh(4, true));
        assert!(!animation.observe_refresh(5, false));
        assert!(animation.frame().is_none());
        assert!(!animation.observe_refresh(5, true));
        assert!(animation.observe_refresh(6, true));
    }
}
