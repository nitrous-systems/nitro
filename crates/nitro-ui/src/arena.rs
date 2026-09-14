//! The widget arena: ids, slots, per-widget framework state and dirty
//! flags.
//!
//! Widgets live in a flat `Vec` of slots and refer to each other by
//! [`WidgetId`] — a generational index. There is no `Rc`, no parent
//! pointer and no interior mutability: a widget is reached only through
//! the arena, which is what lets a callback mutate *another* widget while
//! the first one is running.
//!
//! The slot holds the widget in an `Option`. While a widget's own method
//! runs it is **taken out** of its slot, so the `&mut Ui` a callback
//! receives cannot alias it; asking for it again is
//! [`Error::Busy`](crate::Error::Busy) rather than a panic or a second
//! mutable borrow.

use nitro_core::Rect;
use nitro_wire::types::NodeId;

use crate::layout::{Constraints, LayoutStyle};
use crate::widget::AnyWidget;
use crate::wire::PaintSlot;

/// A handle to a widget in the arena.
///
/// Generational: a recycled slot gets a new generation, so an id kept
/// past its widget's destruction is detected
/// ([`Error::StaleWidget`](crate::Error::StaleWidget)) rather than
/// silently naming whatever moved in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WidgetId {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl WidgetId {
    /// The slot index, for debugging and stable ordering.
    #[must_use]
    pub fn index(self) -> u32 {
        self.index
    }

    /// The generation, which is what makes a stale id detectable.
    #[must_use]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

impl std::fmt::Display for WidgetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}.{}", self.index, self.generation)
    }
}

/// What still has to be done for a widget.
///
/// Three flags say the widget itself needs a pass; the three `SUB_*`
/// flags say a *descendant* does, and are what lets a pass skip a clean
/// subtree without walking it. Marking a widget lights its own flag and
/// then the matching `SUB_` flag on every ancestor, stopping as soon as
/// one is already lit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty(u8);

impl Dirty {
    /// Nothing to do.
    pub const NONE: Self = Self(0);
    /// The widget must be measured and laid out again.
    pub const LAYOUT: Self = Self(1 << 0);
    /// The widget must emit its scene mutations again.
    pub const PAINT: Self = Self(1 << 1);
    /// The widget's children changed: groups to create, reparent or drop.
    pub const TREE: Self = Self(1 << 2);
    /// A descendant has [`Dirty::LAYOUT`].
    pub const SUB_LAYOUT: Self = Self(1 << 3);
    /// A descendant has [`Dirty::PAINT`].
    pub const SUB_PAINT: Self = Self(1 << 4);
    /// A descendant has [`Dirty::TREE`].
    pub const SUB_TREE: Self = Self(1 << 5);

    /// The `SUB_` flag matching an own-widget flag.
    ///
    /// Only the three own-widget bits have a `SUB_` counterpart, so the
    /// input is masked to them first: `SUB_LAYOUT.to_sub()` is
    /// [`Dirty::NONE`], not a pair of bits that mean nothing. `mark` and
    /// the constants are both public, so this has to be total rather
    /// than merely unused.
    #[must_use]
    pub const fn to_sub(self) -> Self {
        Self((self.0 & 0b111) << 3)
    }

    /// Whether **any** of `other`'s bits are set.
    ///
    /// The passes want this one: `has(PAINT | SUB_PAINT)` asks "is there
    /// anything to paint in here?", where either bit is a yes.
    #[must_use]
    pub const fn has(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether **every** one of `other`'s bits is set.
    ///
    /// [`Ui::mark`](crate::Ui::mark)'s early stop wants this one, and the
    /// difference is not academic: a mark of `LAYOUT | PAINT` walks up
    /// setting `SUB_LAYOUT | SUB_PAINT`, and stopping at the first
    /// ancestor that already had *one* of them left the other unset all
    /// the way to the root — so the paint pass skipped a subtree that
    /// held a `PAINT` widget and the change never reached the screen.
    #[must_use]
    pub const fn has_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether nothing is set.
    #[must_use]
    pub const fn is_clean(self) -> bool {
        self.0 == 0
    }

    /// Set `other`'s bits.
    pub const fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// Clear `other`'s bits.
    pub const fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }
}

impl std::ops::BitOr for Dirty {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// Everything the framework knows about a widget, as opposed to what the
/// widget knows about itself.
///
/// It is framework-owned on purpose: layout, painting and hit testing
/// read it for every widget on every pass, and a widget that forgot to
/// expose one of these would simply not work.
#[derive(Debug)]
// Five flags, not a state machine: `focusable`, `hovered`, `focused`
// and the two content-group bits are independent facts about a widget,
// and folding any pair into an enum would invent a state that cannot
// occur to describe two that can.
#[allow(clippy::struct_excessive_bools)]
pub struct WidgetState {
    /// Parent, or `None` for the root.
    pub parent: Option<WidgetId>,
    /// Children, in paint and hit-test order (later children are on top).
    pub children: Vec<WidgetId>,
    /// Flex style; the parent reads it when laying this widget out.
    pub style: LayoutStyle,
    /// Bounds in the parent's coordinate space, written by the layout
    /// pass.
    pub bounds: Rect,
    /// Accessible name, for introspection.
    pub name: Option<String>,
    /// Whether Tab can move focus here.
    pub focusable: bool,
    /// Whether the pointer is inside `bounds`.
    pub hovered: bool,
    /// Whether this widget has the keyboard focus.
    pub focused: bool,
    /// Memoized `measure`: the constraints it was called with and what it
    /// answered. Invalidated by [`Dirty::LAYOUT`].
    pub(crate) measured: Option<(Constraints, nitro_core::Size)>,
    pub(crate) flags: Dirty,
    /// The widget's own scene `Group`; children hang under it.
    pub(crate) node: Option<NodeId>,
    /// Inner `Group` holding the children's groups, created on demand
    /// *after* this widget's own painted nodes so those stay underneath.
    pub(crate) content: Option<NodeId>,
    /// The clip and transform last sent for the content group, so a
    /// scroll that changes nothing costs nothing. The transform is what
    /// makes scrolling one mutation: the children move without being
    /// laid out or painted again.
    pub(crate) content_clip: bool,
    pub(crate) content_transform: nitro_core::Transform,
    /// Where this widget's group was last attached: `(parent, before)`.
    /// Compared before sending a `Reparent`, so a stable tree costs
    /// nothing.
    pub(crate) attached: Option<(NodeId, NodeId)>,
    /// Per-paint-slot scene nodes and the last values sent for them.
    pub(crate) slots: Vec<PaintSlot>,
}

impl Default for WidgetState {
    fn default() -> Self {
        Self {
            parent: None,
            children: Vec::new(),
            style: LayoutStyle::default(),
            bounds: Rect::EMPTY,
            name: None,
            focusable: false,
            hovered: false,
            focused: false,
            measured: None,
            // A new widget has never been laid out, painted or attached.
            flags: Dirty::LAYOUT | Dirty::PAINT | Dirty::TREE,
            node: None,
            content: None,
            content_clip: false,
            content_transform: nitro_core::Transform::IDENTITY,
            attached: None,
            slots: Vec::new(),
        }
    }
}

/// One arena entry.
pub(crate) struct Slot<S> {
    /// `None` while the widget is out — either destroyed (`alive` false)
    /// or currently running one of its own methods.
    pub(crate) widget: Option<Box<dyn AnyWidget<S>>>,
    pub(crate) state: WidgetState,
    pub(crate) generation: u32,
    pub(crate) alive: bool,
}

/// The flat store of widgets.
pub(crate) struct Arena<S> {
    pub(crate) slots: Vec<Slot<S>>,
    free: Vec<u32>,
}

impl<S> Default for Arena<S> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

impl<S: 'static> Arena<S> {
    /// Insert a widget, returning its id.
    pub(crate) fn insert_boxed(
        &mut self,
        widget: Box<dyn AnyWidget<S>>,
        state: WidgetState,
    ) -> WidgetId {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.widget = Some(widget);
            slot.state = state;
            slot.alive = true;
            return WidgetId {
                index,
                generation: slot.generation,
            };
        }
        let index = self.slots.len() as u32;
        self.slots.push(Slot {
            widget: Some(widget),
            state,
            generation: 1,
            alive: true,
        });
        WidgetId {
            index,
            generation: 1,
        }
    }

    /// Free a slot. The id becomes stale; the slot is recycled unless its
    /// generation would overflow, in which case it is retired so an
    /// ancient id can never come back to life.
    pub(crate) fn remove(&mut self, id: WidgetId) -> Option<Box<dyn AnyWidget<S>>> {
        if !self.is_live(id) {
            return None;
        }
        let slot = &mut self.slots[id.index as usize];
        let widget = slot.widget.take();
        slot.alive = false;
        slot.state = WidgetState::default();
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(id.index);
        }
        widget
    }

    /// Whether `id` names a living widget.
    pub(crate) fn is_live(&self, id: WidgetId) -> bool {
        self.slots
            .get(id.index as usize)
            .is_some_and(|s| s.alive && s.generation == id.generation)
    }

    pub(crate) fn slot(&self, id: WidgetId) -> Option<&Slot<S>> {
        self.slots
            .get(id.index as usize)
            .filter(|s| s.alive && s.generation == id.generation)
    }

    pub(crate) fn slot_mut(&mut self, id: WidgetId) -> Option<&mut Slot<S>> {
        self.slots
            .get_mut(id.index as usize)
            .filter(|s| s.alive && s.generation == id.generation)
    }

    /// Number of living widgets.
    pub(crate) fn len(&self) -> usize {
        self.slots.iter().filter(|s| s.alive).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_flags_compose() {
        let mut d = Dirty::NONE;
        assert!(d.is_clean());
        d.insert(Dirty::LAYOUT | Dirty::PAINT);
        assert!(d.has(Dirty::LAYOUT));
        assert!(d.has(Dirty::PAINT));
        assert!(!d.has(Dirty::TREE));
        d.remove(Dirty::LAYOUT);
        assert!(!d.has(Dirty::LAYOUT));
        assert_eq!(Dirty::LAYOUT.to_sub(), Dirty::SUB_LAYOUT);
        assert_eq!(Dirty::PAINT.to_sub(), Dirty::SUB_PAINT);
        assert_eq!(Dirty::TREE.to_sub(), Dirty::SUB_TREE);
        // A `SUB_` flag has no counterpart of its own, so it maps to
        // nothing rather than to a bit outside the set.
        assert_eq!(Dirty::SUB_LAYOUT.to_sub(), Dirty::NONE);
        assert_eq!(Dirty::SUB_TREE.to_sub(), Dirty::NONE);
        assert_eq!(
            (Dirty::LAYOUT | Dirty::SUB_PAINT).to_sub(),
            Dirty::SUB_LAYOUT
        );
    }

    #[test]
    fn has_is_any_and_has_all_is_every() {
        // The distinction `Ui::mark` turns on, and the bug it caused when
        // the two were the same function: a combined mark's early stop
        // must ask "does this ancestor already carry *both* sub flags?",
        // not "does it carry either?".
        let mut d = Dirty::NONE;
        d.insert(Dirty::SUB_LAYOUT);
        let both = Dirty::SUB_LAYOUT | Dirty::SUB_PAINT;
        assert!(d.has(both), "one of the two is set");
        assert!(!d.has_all(both), "but not both");
        d.insert(Dirty::SUB_PAINT);
        assert!(d.has_all(both));
        // Everything has all of nothing, which is what makes the walk
        // terminate rather than loop on an empty mark.
        assert!(Dirty::NONE.has_all(Dirty::NONE));
        assert!(!Dirty::NONE.has(Dirty::PAINT));
    }

    #[test]
    fn ids_are_generational() {
        let mut arena: Arena<()> = Arena::default();
        let a = arena.insert_boxed(Box::new(crate::widgets::Spacer), WidgetState::default());
        assert!(arena.is_live(a));
        arena.remove(a);
        assert!(!arena.is_live(a));
        let b = arena.insert_boxed(Box::new(crate::widgets::Spacer), WidgetState::default());
        assert_eq!(b.index, a.index, "the slot is recycled");
        assert_ne!(b.generation, a.generation, "but not under the same id");
        assert!(!arena.is_live(a));
        assert!(arena.is_live(b));
        assert_eq!(arena.len(), 1);
        assert_eq!(format!("{b}"), "#0.2");
    }
}
