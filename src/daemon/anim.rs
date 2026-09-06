// SPDX-License-Identifier: GPL-3.0-or-later
//! Timed easing for the crossfade. Timing only — interruption is handled by
//! the caller swapping out the whole `Transition`.

use std::time::{Duration, Instant};

/// Which cubic curve [`Anim::eased`] applies to its linear progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    /// `1 − (1 − p)³` — fast start, gentle settle. The overview *opening*.
    Out,
    /// `p³` — gentle start, fast finish. The overview *closing*: the mirror
    /// of [`Ease::Out`], so an open then close reads as one motion played
    /// forward and then back.
    In,
}

/// A cubic ramp from 0 to 1 over `duration`, starting when it is constructed.
#[derive(Debug, Clone, Copy)]
pub struct Anim {
    start: Instant,
    duration: Duration,
    ease: Ease,
}

impl Anim {
    #[must_use]
    pub fn new(duration: Duration, ease: Ease) -> Self {
        Self {
            start: Instant::now(),
            duration,
            ease,
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

    /// Eased progress — the cubic named by [`Ease`]. Both curves are exact at
    /// the `0` and `1` endpoints.
    #[must_use]
    pub fn eased(&self) -> f32 {
        let p = self.progress();
        match self.ease {
            Ease::Out => 1.0 - (1.0 - p).powi(3),
            Ease::In => p.powi(3),
        }
    }

    #[must_use]
    pub fn done(&self) -> bool {
        self.progress() >= 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Anim` of `duration`/`ease` that has already been running for
    /// `elapsed`.
    fn aged(duration: Duration, elapsed: Duration, ease: Ease) -> Anim {
        Anim {
            start: Instant::now()
                .checked_sub(elapsed)
                .unwrap_or_else(Instant::now),
            duration,
            ease,
        }
    }

    #[test]
    fn fresh_animation_is_at_zero() {
        let a = Anim::new(Duration::from_millis(250), Ease::Out);
        assert!(a.progress() < 0.05);
        assert!(a.eased() < 0.05);
        assert!(!a.done());
    }

    #[test]
    fn past_the_end_is_one_and_done() {
        for ease in [Ease::Out, Ease::In] {
            let a = aged(Duration::from_millis(250), Duration::from_millis(400), ease);
            assert_eq!(a.progress(), 1.0);
            assert_eq!(a.eased(), 1.0);
            assert!(a.done());
        }
    }

    #[test]
    fn ease_out_leads_linear_in_the_first_half() {
        let a = aged(
            Duration::from_millis(200),
            Duration::from_millis(100),
            Ease::Out,
        );
        let p = a.progress();
        assert!((0.45..=0.55).contains(&p), "progress {p}");
        assert!(
            a.eased() > p,
            "ease-out {} not ahead of linear {p}",
            a.eased()
        );
        assert!(a.eased() < 1.0);
    }

    #[test]
    fn ease_in_trails_linear_in_the_first_half() {
        let a = aged(
            Duration::from_millis(200),
            Duration::from_millis(100),
            Ease::In,
        );
        let p = a.progress();
        assert!((0.45..=0.55).contains(&p), "progress {p}");
        assert!(a.eased() < p, "ease-in {} not behind linear {p}", a.eased());
        assert!(a.eased() > 0.0);
    }

    #[test]
    fn ease_in_is_the_mirror_of_ease_out() {
        // ease_in(p) == 1 − ease_out(1 − p)
        let d = Duration::from_millis(300);
        for ms in [0, 45, 120, 210, 285, 300] {
            let inn = aged(d, Duration::from_millis(ms), Ease::In).eased();
            let out = aged(d, Duration::from_millis(300 - ms), Ease::Out).eased();
            assert!(
                (inn - (1.0 - out)).abs() < 1e-5,
                "at {ms}ms: in {inn}, out {out}"
            );
        }
    }

    #[test]
    fn zero_duration_is_immediately_done() {
        for ease in [Ease::Out, Ease::In] {
            let a = Anim::new(Duration::ZERO, ease);
            assert_eq!(a.progress(), 1.0);
            assert!(a.done());
            assert_eq!(a.eased(), 1.0);
        }
    }

    #[test]
    fn eased_is_monotonic() {
        for ease in [Ease::Out, Ease::In] {
            let d = Duration::from_millis(300);
            let mut prev = 0.0;
            for ms in [0, 30, 75, 150, 225, 290, 300, 500] {
                let e = aged(d, Duration::from_millis(ms), ease).eased();
                assert!(e >= prev - 1e-6, "{ease:?} dropped at {ms}ms: {e} < {prev}");
                assert!((0.0..=1.0).contains(&e));
                prev = e;
            }
        }
    }
}
