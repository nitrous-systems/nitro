//! `nitro-ui` — the client-side toolkit.
//!
//! A **retained** widget tree in an arena that maps widgets to scene
//! nodes and sends only mutations for what changed. No per-frame rebuild,
//! no virtual tree, no `Rc<RefCell>` anywhere in the public API.
//!
//! ```no_run
//! use nitro_ui::build::{ContainerBuilder as _, StyleBuilder as _};
//! use nitro_ui::widgets::{Label, button, column, label};
//! use nitro_ui::{App, Ui, WidgetId};
//!
//! /// The app's state is a plain struct. Callbacks are handed `&mut` to
//! /// it, so nothing here is shared, reference-counted or cloned.
//! struct State {
//!     clicks: u32,
//!     count: Option<WidgetId>,
//! }
//!
//! # fn main() -> Result<(), nitro_ui::Error> {
//! let state = State {
//!     clicks: 0,
//!     count: None,
//! };
//! App::new("counter")?.run(state, |ui: &mut Ui<State>| {
//!     let count = ui.build(label("0"));
//!     let root = ui.build(
//!         column()
//!             .gap(8.0)
//!             .padding(12.0)
//!             .child(button("Click me").on_click(move |s: &mut State, ui: &mut Ui<State>| {
//!                 s.clicks += 1;
//!                 if let Ok(mut l) = ui.widget_mut::<Label>(count) {
//!                     l.set_text(s.clicks.to_string());
//!                 }
//!             })),
//!     );
//!     ui.attach(root, count).unwrap();
//!     root
//! })
//! # }
//! ```
//!
//! # The five ideas
//!
//! 1. **Retained widgets with dirty flags.** A widget is created once and
//!    lives until it is removed. Work is proportional to what changed:
//!    [`Ui::flush`] walks only the dirty subtrees, and a tree with
//!    nothing dirty sends nothing at all.
//! 2. **[`WidgetMut`] is the only way to mutate a widget.** Its setters
//!    mark layout or paint dirty, so invalidation cannot be forgotten —
//!    there is no other door.
//! 3. **Callbacks take `&mut S` and `&mut Ui<S>`.** The app's state is a
//!    plain struct; a button's `on_click` gets it and the whole tree, and
//!    routes by widget id. No interior mutability, no reference counting.
//! 4. **Builders that build once.** `column().gap(8.0).child(label("Hi"))`
//!    reads like a declarative toolkit and materialises into the arena
//!    exactly once.
//! 5. **Five passes**: `event`, `update`, `layout`, `paint`, `introspect`
//!    — each a tree walk driven by dirty flags.
//!
//! The architecture, the mapping to scene nodes and the reasoning behind
//! each of those is in `docs/ui.md` in the repository.
//!
//! # Modules
//!
//! | module | what is in it |
//! |--------|---------------|
//! | [`widget`] | the [`Widget`] trait and the pass contexts |
//! | [`widgets`] | `Flex`, `Panel`, `Label`, `Button`, `TextField`, `Checkbox`, `Scroll`, `Slider`, `Separator`, `Image`, `Spacer` |
//! | [`list`] | `List`: a **virtualised** list, `visible + 2` nodes for any model |
//! | [`build`] | the builder traits |
//! | [`layout`] | the flex model, as pure functions |
//! | [`event`] | [`Event`], [`Handled`] and the key/button codes |
//! | [`theme`] | [`Theme`] and [`TextStyle`] |
//! | [`introspect`] | the per-app socket: `list`/`get`/`set`/`do`/`watch`/`shot` |
//! | [`shell`] | shell surfaces: layers, anchors, exclusive zones, the window list |
//! | [`shot`] | screenshot one window, by cropping the server's |
//! | [`test`] | the in-process harness (feature `test-support`) |

pub mod app;
pub mod arena;
pub mod build;
pub mod error;
pub mod event;
pub mod introspect;
pub mod layout;
pub mod list;
pub mod shell;
pub mod shot;
pub mod theme;
pub mod ui;
pub mod widget;
pub mod widgets;
mod wire;

#[cfg(feature = "test-support")]
pub mod test;

pub use app::App;
pub use arena::{Dirty, WidgetId, WidgetState};
// The geometry, colour and alignment types this crate's own API is
// written in. Re-exported so an app depends on `nitro-ui` and nothing
// else: `label(..).align(Align::Right)` should not require the caller to
// name — and version-match — the crate the toolkit happens to get its
// `Align` from.
pub use build::{Built, ContainerBuilder, IntoWidget, StyleBuilder};
pub use error::Error;
pub use event::{Event, Handled, KeyEvent};
pub use layout::{
    Constraints, CrossAlign, Direction, Edges, FlexItem, LayoutStyle, Length, MainAlign,
};
pub use list::{List, ListModel, Row, list};
pub use nitro_core::{Color, Point, Rect, Size, Transform};
// The semantic colour palette. `nitro_core::Role` is re-exported as
// `ColorRole` because this crate already has a `Role` — the
// *accessibility* role of a widget ([`widget::Role`]), which predates it
// and is what `Widget::role` returns. Two unrelated things called `Role`
// in one prelude would be a trap for every app that imported both, and
// renaming the accessibility one would churn every widget in the tree for
// the sake of the newer concept.
pub use nitro_core::{Palette, Role as ColorRole, Scheme};
pub use nitro_wire::types::Align;
// A custom widget's `paint` takes a `Fill` ([`PaintCx::rect`]), so a
// crate that writes one — `nitro-wallpaper`'s gradient backdrop is the
// first — must be able to name it. Re-exported for the same reason
// `Align` is: an app depends on `nitro-ui` and nothing else.
pub use nitro_wire::msg::Fill;
pub use shell::{Anchor, ShellEvent, Surface};
pub use theme::{TextStyle, Theme};
pub use ui::{FdToken, Frame, Node, TimerId, Ui, WidgetMut};
pub use widget::{Access, EventCx, LayoutCx, MeasureCx, PaintCx, Role, Slot, TextRun, Widget};
pub use wire::{Mutation, TextMetrics};
