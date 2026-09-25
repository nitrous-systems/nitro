//! A container you can click: what Winamp's clock is.
//!
//! Winamp flips its time display between elapsed and remaining when the
//! clock is clicked. A [`Label`](nitro_ui::widgets::Label) takes no
//! input and a [`Button`](nitro_ui::widgets::Button) draws a face, so
//! the clock is a label inside this: a plain column that takes the
//! pointer events its children let bubble past, and answers `activate`
//! so `hey` can do the same thing a click does.

use nitro_ui::build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
use nitro_ui::event::button;
use nitro_ui::layout::Constraints;
use nitro_ui::widget::{Access, EventCx, MeasureCx, Role, Widget};
use nitro_ui::{Event, Handled, Size, Ui};

/// What a tap does.
type TapFn<S> = Box<dyn Fn(&mut S, &mut Ui<S>)>;

/// The widget.
pub struct Tap<S> {
    on_tap: Option<TapFn<S>>,
}

impl<S: 'static> Tap<S> {
    fn fire(&mut self, cx: &mut EventCx<'_, S>) {
        // Out and back, for the same reason a button does it: the
        // callback gets the whole tree, this widget included.
        if let Some(f) = self.on_tap.take() {
            f(cx.state, cx.ui);
            self.on_tap = Some(f);
        }
    }
}

impl<S: 'static> Widget<S> for Tap<S> {
    fn measure(&mut self, cx: &mut MeasureCx<'_, S>, constraints: Constraints) -> Size {
        // A column's intrinsic size: the widest child, the children's
        // heights stacked. Enough for what this holds (one label), and
        // what the default `layout` then places them in.
        let style = cx.ui.style(cx.id);
        let inner = constraints.loosen().deflate(style.padding);
        let mut size = Size::ZERO;
        let children = cx.children();
        let n = children.len();
        for c in children {
            let s = cx.measure_child(c, inner);
            size.w = size.w.max(s.w);
            size.h += s.h;
        }
        if n > 1 {
            size.h += style.gap * (n - 1) as f32;
        }
        constraints.constrain(Size::new(
            size.w + style.padding.horizontal(),
            size.h + style.padding.vertical(),
        ))
    }

    fn event(&mut self, cx: &mut EventCx<'_, S>, ev: &Event) -> Handled {
        match ev {
            Event::PointerDown { button, .. } if *button == button::LEFT => {
                self.fire(cx);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn accessible(&self) -> Access {
        Access {
            name: None,
            value: None,
            actions: vec!["activate"],
        }
    }

    fn action(&mut self, cx: &mut EventCx<'_, S>, action: &str, _arg: Option<&str>) -> Handled {
        if matches!(action, "activate" | "click") {
            self.fire(cx);
            Handled::Yes
        } else {
            Handled::No
        }
    }
}

/// Builder for a [`Tap`].
pub struct TapBuilder<S> {
    built: Built<S>,
}

impl<S: 'static> TapBuilder<S> {
    /// What a click does.
    #[must_use]
    pub fn on_tap(mut self, f: impl Fn(&mut S, &mut Ui<S>) + 'static) -> Self {
        self.built.replace_widget(Tap {
            on_tap: Some(Box::new(f)),
        });
        self
    }
}

impl<S: 'static> StyleBuilder<S> for TapBuilder<S> {
    fn built_mut(&mut self) -> &mut Built<S> {
        &mut self.built
    }
}

impl<S: 'static> ContainerBuilder<S> for TapBuilder<S> {}

impl<S: 'static> IntoWidget<S> for TapBuilder<S> {
    fn into_widget(self) -> Built<S> {
        self.built
    }
}

/// A clickable column.
#[must_use]
pub fn tap<S: 'static>() -> TapBuilder<S> {
    TapBuilder {
        built: Built::new(Tap::<S> { on_tap: None }),
    }
}
