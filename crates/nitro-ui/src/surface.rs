//! Video and other client-rendered pixels: the `Surface` node path.
//!
//! A [`SurfaceView`] is a widget whose whole box is **one `Surface`
//! node**. Its content does not go through the paint pass at all: the
//! app renders frames into its own buffers (sealed memfds, registered
//! with [`Ui::create_surface_buffer`](crate::Ui::create_surface_buffer))
//! and queues them with [`Ui::present_surface`](crate::Ui::present_surface),
//! which the server latches at vblank. The tree repaints only when the
//! view is resized, so a 60 fps video costs the toolkit nothing per frame.
//!
//! The view's children are an **overlay**: each is measured and pinned
//! over the surface (bottom-aligned by default), and because a widget's
//! own slots are painted in front of its content group, the children
//! land *above* the video — the controls bar of a player.
//!
//! Needs [`Ui::enable_surfaces`](crate::Ui::enable_surfaces) before the
//! window opens; the server's answers arrive as [`SurfaceEvent`]s through
//! [`Ui::on_surface`](crate::Ui::on_surface).

use nitro_core::{Point, Rect, Size};
use nitro_wire::types::{BufferId, NodeId};

use crate::build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
use crate::event::{Event, Handled};
use crate::layout::Constraints;
use crate::ui::Ui;
use crate::widget::{EventCx, LayoutCx, MeasureCx, PaintCx, Role, Widget};

/// What the server said about a presented frame or a surface buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceEvent {
    /// The flip carrying `serial` completed (commits' serials too).
    Presented {
        /// The `PresentSurface` (or `Commit`) serial.
        serial: u32,
        /// Presentation time, `CLOCK_MONOTONIC` nanoseconds.
        time_ns: u64,
        /// Output frame sequence number.
        seq: u64,
    },
    /// The server is done reading a buffer; its pixels may be rewritten.
    Released(BufferId),
    /// The server's preferred format and size for a surface node.
    Hint {
        /// The surface node.
        node: NodeId,
        /// Preferred DRM fourcc.
        format: u32,
        /// Preferred width, device pixels.
        width: u32,
        /// Preferred height, device pixels.
        height: u32,
    },
}

/// Where an overlay child sits over the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayAlign {
    /// Full width, pinned to the bottom edge at its measured height.
    #[default]
    Bottom,
    /// Full width, pinned to the top edge.
    Top,
    /// The whole view.
    Fill,
}

/// A pointer event on the view (not on an overlay child that took it),
/// in the view's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SurfacePointer {
    /// The pointer moved (or entered).
    Move(Point),
    /// A button went down.
    Down(Point, u32),
    /// A button came up.
    Up(Point, u32),
    /// The pointer left.
    Leave,
}

/// A pointer callback; see [`SurfaceViewBuilder::on_pointer`].
type PointerFn<S> = Box<dyn FnMut(&mut S, &mut Ui<S>, SurfacePointer)>;

/// One `Surface` node filling the widget's box, with overlay children.
pub struct SurfaceView<S> {
    node: NodeId,
    size: Size,
    /// Width / height to letterbox to; `None` fills the whole box.
    aspect: Option<f32>,
    /// Where the surface node sits inside the view, as last painted.
    content: Rect,
    align: Vec<OverlayAlign>,
    on_pointer: Option<PointerFn<S>>,
}

impl<S> SurfaceView<S> {
    /// The surface node, once the view has painted; [`NodeId::NONE`]
    /// before. What a `PresentSurface` names.
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// Where the surface node sits inside the view (the letterboxed
    /// rectangle), as last painted.
    #[must_use]
    pub fn content_rect(&self) -> Rect {
        self.content
    }

    /// The view's size as last painted, in logical pixels.
    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }
}

impl<S: 'static> Widget<S> for SurfaceView<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        // As big as it is allowed to be: a video fills what it is given.
        let _ = cx;
        let w = if constraints.max.w.is_finite() {
            constraints.max.w
        } else {
            constraints.min.w
        };
        let h = if constraints.max.h.is_finite() {
            constraints.max.h
        } else {
            constraints.min.h
        };
        constraints.constrain(Size::new(w, h))
    }

    fn layout(&mut self, cx: &mut LayoutCx<'_, S>, bounds: Rect) {
        let (w, h) = (bounds.w, bounds.h);
        for (i, c) in cx.children().into_iter().enumerate() {
            let align = self.align.get(i).copied().unwrap_or_default();
            let r = match align {
                OverlayAlign::Fill => Rect::new(0.0, 0.0, w, h),
                OverlayAlign::Bottom | OverlayAlign::Top => {
                    let m = cx
                        .ui
                        .measure(c, Constraints::loose(Size::new(w, h)))
                        .h
                        .min(h);
                    let y = if align == OverlayAlign::Top {
                        0.0
                    } else {
                        h - m
                    };
                    Rect::new(0.0, y, w, m)
                }
            };
            cx.place_child(c, r);
        }
    }

    fn paint(&mut self, cx: &mut PaintCx<'_, S>) {
        let bounds = cx.bounds;
        self.size = bounds.size();
        let content = fit(bounds, self.aspect);
        self.content = content;
        // Black only in the letterbox strips, never under the picture:
        // a CPU compositor would otherwise fill every video pixel twice.
        let (a, b) = if content.w < bounds.w {
            let l = (content.x - bounds.x).max(0.0);
            (
                Rect::new(bounds.x, bounds.y, l, bounds.h),
                Rect::new(
                    content.x + content.w,
                    bounds.y,
                    bounds.w - l - content.w,
                    bounds.h,
                ),
            )
        } else {
            let t = (content.y - bounds.y).max(0.0);
            (
                Rect::new(bounds.x, bounds.y, bounds.w, t),
                Rect::new(
                    bounds.x,
                    content.y + content.h,
                    bounds.w,
                    bounds.h - t - content.h,
                ),
            )
        };
        if a.w > 0.0 && a.h > 0.0 {
            cx.fill_rect(0, a, nitro_core::Color::BLACK);
        }
        if b.w > 0.0 && b.h > 0.0 {
            cx.fill_rect(1, b, nitro_core::Color::BLACK);
        }
        self.node = cx.surface(2, content);
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        let p = match ev {
            Event::PointerEnter { pos } | Event::PointerMove { pos } => SurfacePointer::Move(*pos),
            Event::PointerDown { pos, button } => SurfacePointer::Down(*pos, *button),
            Event::PointerUp { pos, button } => SurfacePointer::Up(*pos, *button),
            Event::PointerLeave => SurfacePointer::Leave,
            _ => return Handled::No,
        };
        if let Some(f) = &mut self.on_pointer {
            f(cx.state, cx.ui, p);
        }
        // Moves bubble through untouched; a press on the bare video is
        // the view's.
        Handled::from(matches!(
            p,
            SurfacePointer::Down(..) | SurfacePointer::Up(..)
        ))
    }

    fn role(&self) -> Role {
        Role::Image
    }
}

/// The largest rectangle of `aspect` (width / height) centred in
/// `bounds`, whole logical pixels; `bounds` itself for `None`.
#[must_use]
pub fn fit(bounds: Rect, aspect: Option<f32>) -> Rect {
    let Some(ratio) = aspect.filter(|r| r.is_finite() && *r > 0.0) else {
        return bounds;
    };
    if bounds.w <= 0.0 || bounds.h <= 0.0 {
        return bounds;
    }
    let (width, height) = if bounds.w / bounds.h > ratio {
        ((bounds.h * ratio).round(), bounds.h)
    } else {
        (bounds.w, (bounds.w / ratio).round())
    };
    let left = bounds.x + ((bounds.w - width) / 2.0).floor();
    let top = bounds.y + ((bounds.h - height) / 2.0).floor();
    Rect::new(left, top, width, height)
}

/// Setters for a live [`SurfaceView`].
impl<S: 'static> crate::ui::WidgetMut<'_, SurfaceView<S>, S> {
    /// Letterbox the surface to `aspect` (width / height); `None` fills.
    pub fn set_aspect(&mut self, aspect: Option<f32>) {
        if self.aspect != aspect {
            self.aspect = aspect;
            self.request_paint();
        }
    }
}

/// Builder for a [`SurfaceView`].
pub struct SurfaceViewBuilder<S> {
    built: Built<S>,
    align: Vec<OverlayAlign>,
    on_pointer: Option<PointerFn<S>>,
    aspect: Option<f32>,
}

impl<S: 'static> SurfaceViewBuilder<S> {
    /// Letterbox the surface to `aspect` (width / height), with black
    /// bars in the rest of the view.
    #[must_use]
    pub fn aspect(mut self, aspect: f32) -> Self {
        self.aspect = Some(aspect);
        self
    }

    /// Add an overlay child placed by `align`.
    #[must_use]
    pub fn overlay(mut self, c: impl IntoWidget<S>, align: OverlayAlign) -> Self {
        self.align.push(align);
        self.built.push(c.into_widget());
        self
    }

    /// Call `f` for pointer events that reach the view itself: moves
    /// anywhere over it (overlay children let moves bubble), and presses
    /// on the bare surface.
    #[must_use]
    pub fn on_pointer(
        mut self,
        f: impl FnMut(&mut S, &mut Ui<S>, SurfacePointer) + 'static,
    ) -> Self {
        self.on_pointer = Some(Box::new(f));
        self
    }
}

impl<S: 'static> StyleBuilder<S> for SurfaceViewBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for SurfaceViewBuilder<S> {}

impl<S: 'static> IntoWidget<S> for SurfaceViewBuilder<S> {
    fn into_widget(mut self) -> Built<S> {
        self.built.replace_widget(SurfaceView::<S> {
            node: NodeId::NONE,
            size: Size::ZERO,
            aspect: self.aspect,
            content: Rect::EMPTY,
            align: self.align,
            on_pointer: self.on_pointer,
        });
        self.built
    }
}

/// A view that is one `Surface` node, growing to fill its parent.
#[must_use]
pub fn surface_view<S: 'static>() -> SurfaceViewBuilder<S> {
    let placeholder: SurfaceView<S> = SurfaceView {
        node: NodeId::NONE,
        size: Size::ZERO,
        aspect: None,
        content: Rect::EMPTY,
        align: Vec::new(),
        on_pointer: None,
    };
    let mut built = Built::new(placeholder);
    built.state_mut().style.flex_grow = 1.0;
    SurfaceViewBuilder {
        built,
        align: Vec::new(),
        on_pointer: None,
        aspect: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_letterboxes_and_pillarboxes() {
        let b = Rect::new(0.0, 0.0, 200.0, 100.0);
        assert_eq!(fit(b, None), b);
        assert_eq!(fit(b, Some(2.0)), b);
        assert_eq!(fit(b, Some(1.0)), Rect::new(50.0, 0.0, 100.0, 100.0));
        assert_eq!(fit(b, Some(4.0)), Rect::new(0.0, 25.0, 200.0, 50.0));
        assert_eq!(fit(b, Some(0.0)), b);
    }
}
