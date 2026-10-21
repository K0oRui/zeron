//! [`smooth_scroll`] — eased wheel scrolling for a scroll container.
//!
//! A mouse wheel arrives as one large jump per notch (Windows sends lines,
//! macOS mostly pixels), so the container snaps and the eye only sees the start
//! and end. This wrapper takes the notch over and feeds the same travel back
//! over the next few frames, shrinking every step geometrically: ~15% of a
//! notch on the first frame, ~15% of what is left on the next, and so on.
//! Precision-touchpad streams (small per-event pixel deltas) and sideways
//! travel pass straight through untouched.
//!
//! Replays are dispatched as ordinary [`ScrollWheelEvent`]s rather than written
//! into a [`gpui::ScrollHandle`] or [`gpui::ListState`], because that is what
//! keeps every downstream decision intact: the transcript's bottom pin and
//! own-turn hold, list scroll handlers, scroll rails, edge fades, and nested
//! code-block scrollers all react to a real wheel stream, just a finer one.
//!
//! The wrapper listens in the bubble phase and registers *after* its child has
//! painted, so it runs before the container's own wheel handler (listeners
//! bubble in reverse registration order) and can hold the event back. Nested
//! wrappers resolve innermost-first for the same reason.

use std::cell::Cell;
use std::time::Duration;

use gpui::{
    AnyElement, App, AsyncApp, Bounds, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, IntoElement, LayoutId, Pixels, PlatformInput, ScrollDelta,
    ScrollWheelEvent, TouchPhase, Window, point, px,
};

/// Replay cadence. Faster than a 60Hz frame so the eased steps stay small
/// enough to read as motion rather than a slowed-down snap.
const FRAME: Duration = Duration::from_millis(8);

/// Time constant of the exponential chase, in seconds. ~95% of a notch lands
/// in 150ms.
const TIME_CONSTANT: f32 = 0.05;

/// Per-event travel below this is already continuous (a trackpad stream), so
/// it keeps the platform's own scrolling.
const SMOOTH_MIN_PIXELS: f32 = 6.0;

/// Leftovers this small are invisible; stop rather than chase them.
const MIN_REMAINDER: f32 = 0.3;

/// One notch is one glide; a fast burst queues a few, and they sum while each
/// fades out on its own.
const MAX_PENDING_PIXELS: f32 = 2400.0;

thread_local! {
    /// Set while a replay is on its way through `Window::dispatch_event`. Every
    /// wrapper shares it, so a replay can never be intercepted again by an
    /// outer wrapper on its way down.
    static REPLAYING: Cell<bool> = const { Cell::new(false) };
}

/// Wrap the scroll container `child` so wheel travel eases in over a few frames.
pub fn smooth_scroll(child: impl IntoElement) -> SmoothScroll {
    SmoothScroll {
        child: child.into_any_element(),
    }
}

/// The share of `remaining` to apply after `elapsed`, and what stays owed.
fn glide_step(remaining: f32, elapsed: Duration) -> (f32, f32) {
    let step = remaining * (1.0 - (-elapsed.as_secs_f32() / TIME_CONSTANT).exp());
    let left = remaining - step;
    (step, if left.abs() < MIN_REMAINDER { 0.0 } else { left })
}

pub struct SmoothScroll {
    child: AnyElement,
}

impl Element for SmoothScroll {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.child.prepaint(window, cx);
        (!crate::motion::reduced_motion(cx))
            .then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let Some(hitbox) = prepaint.clone() else {
            return;
        };
        window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || cx.has_active_drag() {
                return;
            }
            let replaying = REPLAYING.with(Cell::get);
            if replaying {
                return;
            }
            if !hitbox.should_handle_scroll(window) {
                return;
            }
            let pixels = event.delta.pixel_delta(window.line_height());
            let (dx, dy) = (f32::from(pixels.x), f32::from(pixels.y));
            // Sideways travel belongs to nested horizontal scrollers, and a
            // small per-event delta is already a fine-grained stream (a
            // precision touchpad) that this easing would only delay.
            if dx.abs() > dy.abs() || dy.abs() < SMOOTH_MIN_PIXELS {
                return;
            }
            cx.stop_propagation();
            let mut remaining = dy.clamp(-MAX_PENDING_PIXELS, MAX_PENDING_PIXELS);
            let window_handle = window.window_handle();
            cx.spawn(async move |cx: &mut AsyncApp| {
                while remaining != 0.0 {
                    cx.background_executor().timer(FRAME).await;
                    let (step, left) = glide_step(remaining, FRAME);
                    remaining = left;
                    if step != 0.0 && !replay(cx, window_handle, bounds, step) {
                        return;
                    }
                }
            })
            .detach();
        });
    }
}

impl IntoElement for SmoothScroll {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Push one eased slice back through the window's normal wheel dispatch.
/// Returns false when the pointer left the wrapper (or the window is gone), in
/// which case the glide is abandoned rather than scrolling someone else.
fn replay(
    cx: &mut AsyncApp,
    window_handle: gpui::AnyWindowHandle,
    bounds: Bounds<Pixels>,
    dy: f32,
) -> bool {
    REPLAYING.with(|flag| flag.set(true));
    let result = window_handle.update(cx, |_, window, cx| {
        let position = window.mouse_position();
        if !bounds.contains(&position) {
            return false;
        }
        window.dispatch_event(
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(px(0.0), px(dy))),
                modifiers: window.modifiers(),
                touch_phase: TouchPhase::Moved,
            }),
            cx,
        );
        true
    });
    REPLAYING.with(|flag| flag.set(false));
    result.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glide_drains_geometrically_and_settles() {
        let mut remaining = 100.0;
        let mut applied = 0.0;
        let mut frames = 0;
        while remaining != 0.0 && frames < 200 {
            let (step, left) = glide_step(remaining, FRAME);
            applied += step;
            remaining = left;
            frames += 1;
        }
        assert_eq!(remaining, 0.0);
        assert!((applied - 100.0).abs() < 0.5, "applied {applied}");
        assert!(frames < 60, "settled in {frames} frames");
    }

    #[test]
    fn glide_first_step_is_a_small_share_of_the_notch() {
        let (step, left) = glide_step(100.0, FRAME);
        assert!(step > 10.0 && step < 20.0, "step {step}");
        assert!(left > 80.0);
    }

    #[test]
    fn a_finished_glide_never_reverses() {
        let (_, left) = glide_step(MIN_REMAINDER * 0.5, FRAME);
        assert_eq!(left, 0.0);
    }
}
