//! The axis of one trackpad swipe (dopamine #1014).
//!
//! `scroll_predominant_axis` used to decide per event (`|dy| >= |dx|`). A vertical swipe
//! carries horizontal-only events (finger wobble, momentum), and each of those scrolled
//! sideways on its own. This decides once per swipe instead: at `TouchPhase::Started`
//! (or after a gap), from the first event that moves, and keeps that axis for the rest
//! of the swipe, momentum included. Mouse wheels (line deltas, no phases) keep the old
//! per-event rule.
//!
//! The same rule as dopamine's `files/axis_lock.rs` (`GestureAxis`, #1012).

use gpui::{Axis, Pixels, Point, TouchPhase, px};
use std::time::{Duration, Instant};

/// A gap this long between events starts a new swipe even without `Started`.
const GESTURE_GAP: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GestureAxis {
    locked: Option<Axis>,
    last: Option<Instant>,
}

impl GestureAxis {
    /// Which axis this event scrolls. `None` means "nothing to do" (zero delta).
    pub(crate) fn decide(
        &mut self,
        phase: TouchPhase,
        d: Point<Pixels>,
        precise: bool,
        now: Instant,
    ) -> Option<Axis> {
        if !precise {
            return predominant(d);
        }
        let gap = self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) >= GESTURE_GAP);
        self.last = Some(now);
        if matches!(phase, TouchPhase::Started) || gap {
            self.locked = None;
        }
        if d.x == px(0.) && d.y == px(0.) {
            return None;
        }
        if self.locked.is_none() {
            self.locked = Some(if d.x == px(0.) || d.y.abs() >= d.x.abs() {
                Axis::Vertical
            } else {
                Axis::Horizontal
            });
        }
        self.locked
    }
}

/// The per-event rule (mouse wheels). A one-axis delta passes through.
fn predominant(d: Point<Pixels>) -> Option<Axis> {
    if d.x == px(0.) && d.y == px(0.) {
        return None;
    }
    Some(if d.y.abs() >= d.x.abs() {
        Axis::Vertical
    } else {
        Axis::Horizontal
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::point;

    fn at(ms: u64) -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now) + Duration::from_millis(ms)
    }

    #[test]
    fn horizontal_only_jitter_in_a_vertical_swipe_stays_vertical() {
        let mut g = GestureAxis::default();
        assert_eq!(g.decide(TouchPhase::Started, point(px(0.), px(-12.)), true, at(0)), Some(Axis::Vertical));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(-2.), px(0.)), true, at(16)), Some(Axis::Vertical));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(-9.), px(-1.)), true, at(32)), Some(Axis::Vertical));
    }

    #[test]
    fn momentum_keeps_the_axis_of_the_swipe() {
        let mut g = GestureAxis::default();
        g.decide(TouchPhase::Started, point(px(0.), px(-12.)), true, at(0));
        g.decide(TouchPhase::Ended, point(px(0.), px(0.)), true, at(16));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(-3.), px(0.)), true, at(32)), Some(Axis::Vertical));
    }

    #[test]
    fn a_new_swipe_or_a_long_gap_decides_again() {
        let mut g = GestureAxis::default();
        g.decide(TouchPhase::Started, point(px(0.), px(-12.)), true, at(0));
        assert_eq!(g.decide(TouchPhase::Started, point(px(-8.), px(0.)), true, at(500)), Some(Axis::Horizontal));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(0.), px(-6.)), true, at(516)), Some(Axis::Horizontal));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(0.), px(-6.)), true, at(900)), Some(Axis::Vertical));
    }

    #[test]
    fn a_mouse_wheel_decides_per_event() {
        let mut g = GestureAxis::default();
        assert_eq!(g.decide(TouchPhase::Moved, point(px(0.), px(-20.)), false, at(0)), Some(Axis::Vertical));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(-20.), px(0.)), false, at(10)), Some(Axis::Horizontal));
        assert_eq!(g.decide(TouchPhase::Moved, point(px(0.), px(0.)), false, at(20)), None);
    }
}
