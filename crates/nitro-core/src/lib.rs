//! Shared value types for every nitro crate: geometry, colour, the
//! semantic colour [`palette`], 2-D affine transforms and damage regions.
//!
//! Zero dependencies, no I/O, no allocation except in [`Damage`]. Everything
//! is `Copy` where it can be. Logical coordinates are `f32`; pixel
//! coordinates are `i32` ([`IRect`]).

#![forbid(unsafe_code)]

mod color;
mod damage;
mod geom;
pub mod palette;
mod transform;

pub use color::Color;
pub use damage::Damage;
pub use geom::{IRect, Point, Rect, Size};
pub use palette::{Palette, Role, Scheme};
pub use transform::Transform;
