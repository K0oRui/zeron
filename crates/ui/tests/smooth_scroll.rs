//! The eased wheel wrapper feeds a single notch back over several frames, and
//! leaves continuous input (precision touchpads, horizontal travel) alone.

use std::time::Duration;

use gpui::{
    Context, Modifiers, Render, ScrollDelta, ScrollHandle, ScrollWheelEvent, TestAppContext,
    TouchPhase, Window, div, point, px, prelude::*,
};
use zeron_ui::smooth_scroll::smooth_scroll;

struct Fixture {
    scroll: ScrollHandle,
}

impl Render for Fixture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size(px(200.0)).child(smooth_scroll(
            div()
                .id("list")
                .size_full()
                .overflow_y_scroll()
                .track_scroll(&self.scroll)
                .child(div().h(px(1000.0))),
        ))
    }
}

fn wheel(delta: ScrollDelta) -> ScrollWheelEvent {
    ScrollWheelEvent {
        position: point(px(100.0), px(100.0)),
        delta,
        modifiers: Modifiers::default(),
        touch_phase: TouchPhase::Moved,
    }
}

fn scrolled(scroll: &ScrollHandle) -> f32 {
    -f32::from(scroll.offset().y)
}

#[gpui::test]
fn a_mouse_notch_glides_over_several_frames(cx: &mut TestAppContext) {
    let clock = cx.background_executor.clone();
    let scroll = ScrollHandle::new();
    let (_, cx) = cx.add_window_view(|_, _| Fixture {
        scroll: scroll.clone(),
    });
    cx.update(|window, cx| window.draw(cx).clear());
    let notch = 3.0 * f32::from(cx.update(|window, _| window.line_height()));
    assert!(notch > 0.0);

    // Negative y is one notch down; positive would clamp against the top.
    cx.simulate_event(wheel(ScrollDelta::Lines(point(0.0, -3.0))));
    assert_eq!(
        scrolled(&scroll),
        0.0,
        "the notch is queued, not applied in one jump"
    );

    let mut frames = 0;
    let mut first_move = None;
    while scrolled(&scroll) < notch - 0.01 && frames < 120 {
        clock.advance_clock(Duration::from_millis(8));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear());
        frames += 1;
        let moved = scrolled(&scroll);
        if moved > 0.0 && first_move.is_none() {
            first_move = Some(moved);
        }
    }

    let total = scrolled(&scroll);
    assert!(
        (total - notch).abs() < 0.5,
        "the whole notch lands: {total} of {notch}"
    );
    let first_move = first_move.expect("the notch moves at all");
    assert!(
        first_move < notch * 0.6,
        "the first frame eases in: {first_move} of {notch}"
    );
    assert!(frames > 3, "the notch is spread over frames: {frames}");
}

#[gpui::test]
fn a_touchpad_stream_scrolls_immediately(cx: &mut TestAppContext) {
    let scroll = ScrollHandle::new();
    let (_, cx) = cx.add_window_view(|_, _| Fixture {
        scroll: scroll.clone(),
    });
    cx.update(|window, cx| window.draw(cx).clear());

    cx.simulate_event(wheel(ScrollDelta::Pixels(point(px(0.0), px(-3.0)))));
    cx.update(|window, cx| window.draw(cx).clear());
    assert_eq!(scroll.offset().y, px(-3.0), "small deltas are not queued");
}

#[gpui::test]
fn horizontal_travel_scrolls_immediately(cx: &mut TestAppContext) {
    let scroll = ScrollHandle::new();
    let (_, cx) = cx.add_window_view(|_, _| Fixture {
        scroll: scroll.clone(),
    });
    cx.update(|window, cx| window.draw(cx).clear());

    cx.simulate_event(wheel(ScrollDelta::Pixels(point(px(-40.0), px(-2.0)))));
    cx.update(|window, cx| window.draw(cx).clear());
    assert_eq!(scroll.offset().y, px(-2.0));
}
