//! The server's retained scene graph: a tree of primitive nodes grouped into
//! windows, with exact damage tracking, a flat paint list and hit testing.
//!
//! Pure data plus bookkeeping — no I/O, no rasterization, no dependency on the
//! wire protocol. The server translates client ids into the keys minted here.
//!
//! The whole crate exists to make *work proportional to change* true:
//!
//! * Every mutation marks the smallest set of nodes affected and lights a
//!   trail of [`Dirty::SUBTREE`] flags up to the owning window's root.
//! * [`Scene::update`] walks only those trails, recomputes world transforms
//!   and device bounds top-down, and emits **old ∪ new** bounds as damage.
//!   An update with nothing dirty visits nothing and damages nothing.
//! * [`Scene::paint_list`] and [`Scene::hit_test`] skip any subtree whose
//!   cached extent misses the region or point they were given.
//!
//! ```
//! use nitro_core::{Color, Damage, IRect, Point, Rect, Size};
//! use nitro_scene::{ClientId, DamageSink, Fill, Layer, NodeKind, OutputId, Scene};
//!
//! let mut scene = Scene::new();
//! let output = OutputId(0);
//! scene.add_output(output, IRect::new(0, 0, 800, 600), 1.0);
//!
//! let client = ClientId(1);
//! let win = scene.create_window(client, "demo", Size::new(400.0, 300.0), Layer::Normal);
//! scene.place_window(win, Some(output), Point::new(10.0, 10.0)).unwrap();
//! let root = scene.window_info(win).unwrap().root();
//!
//! let rect = scene.create_node(client, NodeKind::Rect, root, None).unwrap();
//! scene.set_bounds(client, rect, Rect::new(0.0, 0.0, 100.0, 50.0)).unwrap();
//! scene.set_fill(client, rect, Fill::Solid(Color::WHITE)).unwrap();
//!
//! let mut damage = Damage::new();
//! scene.update(&mut DamageSink::new(&mut [(output, &mut damage)]));
//! assert_eq!(damage.bounds(), IRect::new(10, 10, 100, 50));
//!
//! // Nothing changed since: no work, no damage.
//! damage.clear();
//! let result = scene.update(&mut DamageSink::new(&mut [(output, &mut damage)]));
//! assert!(damage.is_empty());
//! assert_eq!(result.stats.visited_nodes, 0);
//! ```
//!
//! # Coordinate spaces
//!
//! Client-facing properties are *logical*: [`Rect`](nitro_core::Rect)s and
//! [`Transform`](nitro_core::Transform)s in `f32`, relative to the parent.
//! Everything cached and everything emitted — world bounds, clips, damage,
//! hit-test input — is in *device pixels*: `i32`, global across all outputs.
//! The conversion happens once, in the window root's transform, which is
//! `output.rect.origin + window.position * output.scale` followed by a scale
//! of `output.scale`. A node's `world_transform` therefore already contains
//! the output's scale, and the rasterizer never needs to know about it.

#![forbid(unsafe_code)]

mod buffer;
mod error;
mod key;
mod node;
mod paint;
mod scene;
mod update;
mod window;

pub use buffer::{Buffer, BufferDesc, BufferKey};
pub use error::Error;
pub use node::{Border, Dirty, Fill, ImageRef, Node, NodeKey, NodeKind, RectData};
pub use paint::{Hit, PaintItem, PaintKind};
pub use scene::{MAX_DEPTH, Scene, UpdateStats};
pub use update::{DamageSink, UpdateResult};
pub use window::{ClientId, Configure, Layer, OutputId, Window, WindowKey};
