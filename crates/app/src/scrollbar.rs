//! A minimal always-visible vertical scrollbar (gpui itself only provides the
//! scrolling, not the bar).
//!
//! Usage: give the scroll container `.track_scroll(&state.handle)`, overlay
//! [`Scrollbar::render`] on it, and forward mouse moves / ups from a parent
//! that covers the whole window so dragging continues outside the bar.

use gpui::{
    App, Div, MouseButton, MouseDownEvent, MouseMoveEvent, Pixels, ScrollHandle, Window, div,
    point, prelude::*, px, rgb,
};

const WIDTH: f32 = 10.0;
const MIN_THUMB: f32 = 28.0;

#[derive(Default)]
pub struct Scrollbar {
    pub handle: ScrollHandle,
    /// While dragging: distance between the cursor and the top of the thumb.
    drag: Option<Pixels>,
}

struct Geometry {
    track_top: Pixels,
    track_height: Pixels,
    thumb_top: Pixels,
    thumb_height: Pixels,
    max_scroll: Pixels,
}

impl Scrollbar {
    fn geometry(&self) -> Option<Geometry> {
        let bounds = self.handle.bounds();
        let max_scroll = self.handle.max_offset().height;
        let track_height = bounds.size.height;
        if max_scroll <= px(0.) || track_height <= px(0.) {
            return None;
        }
        let content = track_height + max_scroll;
        let thumb_height = (track_height * (track_height / content))
            .max(px(MIN_THUMB))
            .min(track_height);
        // gpui offsets are negative when scrolled down.
        let scrolled = (-self.handle.offset().y).clamp(px(0.), max_scroll);
        let thumb_top = (track_height - thumb_height) * (scrolled / max_scroll);
        Some(Geometry {
            track_top: bounds.origin.y,
            track_height,
            thumb_top,
            thumb_height,
            max_scroll,
        })
    }

    /// Scrolls so that the thumb's top edge lands at `thumb_top` (relative to the track).
    fn scroll_thumb_to(&self, g: &Geometry, thumb_top: Pixels) {
        let free = g.track_height - g.thumb_height;
        if free <= px(0.) {
            return;
        }
        let ratio = (thumb_top / free).clamp(0., 1.);
        let x = self.handle.offset().x;
        self.handle.set_offset(point(x, -(g.max_scroll * ratio)));
    }

    /// Mouse down on the bar: grab the thumb, or jump when clicking the track.
    pub fn mouse_down(&mut self, event: &MouseDownEvent) {
        let Some(g) = self.geometry() else { return };
        let y = event.position.y - g.track_top;
        if y >= g.thumb_top && y <= g.thumb_top + g.thumb_height {
            self.drag = Some(y - g.thumb_top);
        } else {
            let grab = g.thumb_height / 2.;
            self.scroll_thumb_to(&g, y - grab);
            self.drag = Some(grab);
        }
    }

    /// Returns true if the view needs to be redrawn.
    pub fn mouse_move(&mut self, event: &MouseMoveEvent) -> bool {
        let Some(grab) = self.drag else { return false };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return true;
        }
        let Some(g) = self.geometry() else {
            return false;
        };
        self.scroll_thumb_to(&g, event.position.y - g.track_top - grab);
        true
    }

    pub fn mouse_up(&mut self) -> bool {
        self.drag.take().is_some()
    }

    /// The bar, absolutely positioned at the right edge of its (relative) parent.
    /// `on_down` should call [`Scrollbar::mouse_down`] and notify.
    pub fn render(
        &self,
        on_down: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Option<Div> {
        let g = self.geometry()?;
        let thumb_color = if self.drag.is_some() {
            0x6a6f78
        } else {
            0x4e535b
        };
        Some(
            div()
                .absolute()
                .top_0()
                .right_0()
                .h(g.track_height)
                .w(px(WIDTH))
                .bg(rgb(0x232427))
                .on_mouse_down(MouseButton::Left, on_down)
                .child(
                    div()
                        .absolute()
                        .left(px(2.))
                        .right(px(2.))
                        .top(g.thumb_top)
                        .h(g.thumb_height)
                        .rounded_full()
                        .bg(rgb(thumb_color))
                        .hover(|s| s.bg(rgb(0x6a6f78))),
                ),
        )
    }
}
