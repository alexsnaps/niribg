// SPDX-License-Identifier: GPL-3.0-or-later
//! Timed easing for the crossfade. Timing only — interruption is handled by
//! the caller swapping out the whole `Transition`.

use std::time::{Duration, Instant};

/// An ease-out-cubic ramp from 0 to 1 over `duration`, starting when it is
/// constructed.
#[derive(Debug, Clone, Copy)]
pub struct Anim {
    start: Instant,
    duration: Duration,
}

impl Anim {
    #[must_use]
    pub fn new(duration: Duration) -> Self {
        Self {
            start: Instant::now(),
            duration,
        }
    }

    /// Linear progress, clamped to `0.0..=1.0`. A zero-length animation is
    /// already complete.
    #[must_use]
    pub fn progress(&self) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (self.start.elapsed().as_secs_f32() / self.duration.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// Eased progress — ease-out cubic (`1 - (1 - p)^3`): fast start, gentle
    /// settle.
    #[must_use]
    pub fn eased(&self) -> f32 {
        let p = self.progress();
        1.0 - (1.0 - p).powi(3)
    }

    #[must_use]
    pub fn done(&self) -> bool {
        self.progress() >= 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Anim` of `duration` that has already been running for `elapsed`.
    fn aged(duration: Duration, elapsed: Duration) -> Anim {
        Anim {
            start: Instant::now()
                .checked_sub(elapsed)
                .unwrap_or_else(Instant::now),
            duration,
        }
    }

    #[test]
    fn fresh_animation_is_at_zero() {
        let a = Anim::new(Duration::from_millis(250));
        assert!(a.progress() < 0.05);
        assert!(a.eased() < 0.05);
        assert!(!a.done());
    }

    #[test]
    fn past_the_end_is_one_and_done() {
        let a = aged(Duration::from_millis(250), Duration::from_millis(400));
        assert_eq!(a.progress(), 1.0);
        assert_eq!(a.eased(), 1.0);
        assert!(a.done());
    }

    #[test]
    fn midpoint_is_between_and_ahead_of_linear() {
        let a = aged(Duration::from_millis(200), Duration::from_millis(100));
        let p = a.progress();
        assert!((0.45..=0.55).contains(&p), "progress {p}");
        // ease-out is ahead of linear in the first half
        assert!(a.eased() > p, "eased {} not > linear {p}", a.eased());
        assert!(a.eased() < 1.0);
    }

    #[test]
    fn zero_duration_is_immediately_done() {
        let a = Anim::new(Duration::ZERO);
        assert_eq!(a.progress(), 1.0);
        assert!(a.done());
        assert_eq!(a.eased(), 1.0);
    }

    #[test]
    fn eased_is_monotonic() {
        let d = Duration::from_millis(300);
        let mut prev = 0.0;
        for ms in [0, 30, 75, 150, 225, 290, 300, 500] {
            let e = aged(d, Duration::from_millis(ms)).eased();
            assert!(e >= prev - 1e-6, "eased dropped at {ms}ms: {e} < {prev}");
            assert!((0.0..=1.0).contains(&e));
            prev = e;
        }
    }
}
