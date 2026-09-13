//! Builder construction: `column().gap(8.0).child(label("Hello"))`.
//!
//! A builder describes a subtree; [`Ui::build`](crate::Ui::build)
//! materialises it into the arena **once**. There is no virtual tree and
//! no diffing: after the build, the only way anything changes is
//! [`WidgetMut`](crate::WidgetMut). That is the whole trade — the
//! construction syntax reads like a declarative toolkit, the runtime
//! behaves like a retained one.

use crate::arena::WidgetState;
use crate::layout::{CrossAlign, Direction, Edges, Length, MainAlign};
use crate::widget::AnyWidget;

/// A widget plus its framework state and children, ready to be inserted.
pub struct Built<S> {
    pub(crate) widget: Box<dyn AnyWidget<S>>,
    pub(crate) state: WidgetState,
    pub(crate) children: Vec<Built<S>>,
}

impl<S: 'static> Built<S> {
    /// A leaf with the default state.
    pub fn new(widget: impl AnyWidget<S>) -> Self {
        Self {
            widget: Box::new(widget),
            state: WidgetState::default(),
            children: Vec::new(),
        }
    }

    /// Swap the widget, keeping the state and children.
    ///
    /// Builders use it because they accumulate style setters (which touch
    /// the state) and widget setters (which touch the widget) in any
    /// order, and only know the final widget at `into_widget` time.
    pub fn replace_widget(&mut self, widget: impl AnyWidget<S>) {
        self.widget = Box::new(widget);
    }

    /// The widget's mutable framework state, for a builder's setters.
    pub fn state_mut(&mut self) -> &mut WidgetState {
        &mut self.state
    }

    /// Append a child subtree.
    pub fn push(&mut self, child: Built<S>) {
        self.children.push(child);
    }
}

/// Anything that can become a widget subtree.
///
/// Implemented by every builder in [`widgets`](crate::widgets), and by
/// [`Built`] itself so a subtree can be passed around.
pub trait IntoWidget<S> {
    /// Turn this into a subtree.
    fn into_widget(self) -> Built<S>;
}

impl<S> IntoWidget<S> for Built<S> {
    fn into_widget(self) -> Built<S> {
        self
    }
}

/// The style setters every builder shares.
///
/// A builder implements [`StyleBuilder::built_mut`] and gets `.width()`,
/// `.padding()`, `.grow()` and the rest for free, so the layout
/// vocabulary is identical on a `Label` and on a `Flex`.
pub trait StyleBuilder<S>: Sized {
    /// The subtree being built.
    fn built_mut(&mut self) -> &mut Built<S>;

    /// Set an explicit width.
    #[must_use]
    fn width(mut self, w: f32) -> Self {
        self.built_mut().state.style.width = Length::Px(w);
        self
    }

    /// Set an explicit height.
    #[must_use]
    fn height(mut self, h: f32) -> Self {
        self.built_mut().state.style.height = Length::Px(h);
        self
    }

    /// Set the width as a fraction of the parent's content box.
    #[must_use]
    fn width_percent(mut self, f: f32) -> Self {
        self.built_mut().state.style.width = Length::Percent(f);
        self
    }

    /// Set the height as a fraction of the parent's content box.
    #[must_use]
    fn height_percent(mut self, f: f32) -> Self {
        self.built_mut().state.style.height = Length::Percent(f);
        self
    }

    /// Clamp the width from below.
    #[must_use]
    fn min_width(mut self, v: f32) -> Self {
        self.built_mut().state.style.min_width = Some(v);
        self
    }

    /// Clamp the width from above.
    #[must_use]
    fn max_width(mut self, v: f32) -> Self {
        self.built_mut().state.style.max_width = Some(v);
        self
    }

    /// Clamp the height from below.
    #[must_use]
    fn min_height(mut self, v: f32) -> Self {
        self.built_mut().state.style.min_height = Some(v);
        self
    }

    /// Clamp the height from above.
    #[must_use]
    fn max_height(mut self, v: f32) -> Self {
        self.built_mut().state.style.max_height = Some(v);
        self
    }

    /// Space inside this widget, around its children.
    #[must_use]
    fn padding(mut self, v: f32) -> Self {
        self.built_mut().state.style.padding = Edges::all(v);
        self
    }

    /// Horizontal and vertical padding.
    #[must_use]
    fn padding_xy(mut self, h: f32, v: f32) -> Self {
        self.built_mut().state.style.padding = Edges::symmetric(h, v);
        self
    }

    /// Space outside this widget, reserved by its parent.
    #[must_use]
    fn margin(mut self, v: f32) -> Self {
        self.built_mut().state.style.margin = Edges::all(v);
        self
    }

    /// Share of the parent's leftover main-axis space.
    #[must_use]
    fn grow(mut self, v: f32) -> Self {
        self.built_mut().state.style.flex_grow = v;
        self
    }

    /// Share of the parent's main-axis overflow this widget gives back.
    #[must_use]
    fn shrink(mut self, v: f32) -> Self {
        self.built_mut().state.style.flex_shrink = v;
        self
    }

    /// Set the accessible name, for introspection.
    #[must_use]
    fn name(mut self, n: impl Into<String>) -> Self {
        self.built_mut().state.name = Some(n.into());
        self
    }
}

/// The container setters, shared by [`Flex`](crate::widgets::Flex) and
/// [`Panel`](crate::widgets::Panel).
pub trait ContainerBuilder<S: 'static>: StyleBuilder<S> {
    /// Append a child.
    #[must_use]
    fn child(mut self, c: impl IntoWidget<S>) -> Self {
        let built = c.into_widget();
        self.built_mut().push(built);
        self
    }

    /// Append several children.
    #[must_use]
    fn children<I>(mut self, cs: I) -> Self
    where
        I: IntoIterator,
        I::Item: IntoWidget<S>,
    {
        for c in cs {
            let built = c.into_widget();
            self.built_mut().push(built);
        }
        self
    }

    /// Space between adjacent children.
    #[must_use]
    fn gap(mut self, v: f32) -> Self {
        self.built_mut().state.style.gap = v;
        self
    }

    /// Stacking direction.
    #[must_use]
    fn direction(mut self, d: Direction) -> Self {
        self.built_mut().state.style.direction = d;
        self
    }

    /// Main-axis distribution.
    #[must_use]
    fn main_align(mut self, a: MainAlign) -> Self {
        self.built_mut().state.style.main_align = a;
        self
    }

    /// Cross-axis placement.
    #[must_use]
    fn cross_align(mut self, a: CrossAlign) -> Self {
        self.built_mut().state.style.cross_align = a;
        self
    }
}
