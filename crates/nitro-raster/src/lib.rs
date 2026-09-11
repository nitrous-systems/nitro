//! `nitro-raster` — the CPU 2-D rasterizer the nitro server paints damaged
//! rects with.
//!
//! One type does the work: [`Canvas`], a mutable borrow of an XRGB8888 back
//! buffer (`[B, G, R, X]` per pixel, exactly what `nitro-kms` dumb buffers
//! want). Every call takes a `clip: &IRect` — the damage rect — and **never**
//! writes a byte outside it.
//!
//! ```
//! use nitro_core::{Color, IRect, Rect};
//! use nitro_raster::{Canvas, Fill};
//!
//! let mut buf = vec![0u8; 64 * 64 * 4];
//! let mut c = Canvas::new(&mut buf, 64, 64, 64 * 4);
//! let damage = IRect::new(8, 8, 32, 32);
//! c.fill_rect(
//!     &damage,
//!     &Rect::new(10.5, 10.5, 20.0, 12.0),
//!     &Fill::Solid(Color::rgb(0x30, 0x80, 0xE0)),
//!     6.0,
//!     1.0,
//! );
//! ```
//!
//! # Contract
//!
//! - **Coordinate space.** Everything is *device* pixels. Pixel `(x, y)`
//!   covers the square `[x, x+1) × [y, y+1)`; its centre is `(x+0.5, y+0.5)`.
//!   The scene applies its transforms before calling: in M1 transforms are
//!   axis-aligned (translate + scale), so shapes arrive as plain [`Rect`]s.
//!   **Rotation is not supported** and is not planned for this crate — a
//!   rotated node needs a path rasterizer, which is a later decision.
//! - **Clipping.** `clip` is intersected with the surface bounds; painting
//!   outside the result is a bug, and the test suite asserts it with a
//!   sentinel-filled canvas.
//! - **Anti-aliasing.** Analytic: the coverage of a pixel is the area of the
//!   pixel square inside the shape. Straight edges are computed exactly;
//!   corner arcs integrate x exactly over 8 sub-scanlines of y, so the
//!   approximation error is confined to the `r × r` corner boxes (a quarter
//!   disc's total coverage lands within 0.2 px² of `πr²/4`).
//! - **Blending.** Source-over with straight-alpha inputs:
//!
//!   ```text
//!   out = round((src * a + dst * (255 - a)) / 255)
//!   a   = round(src.a * coverage * opacity / 255²)
//!   ```
//!
//!   The division by 255 is the exact `(t + (t >> 8)) >> 8` trick with
//!   `t = x + 128`, which equals `round(x / 255)` for every value we produce.
//!   A float reference implementation is in the test module; the two agree to
//!   within ±1 on random input.
//! - **Colour space.** sRGB *bytes are blended as-is*, with no linearization.
//!   This is a deliberate M1 simplification: it is what every toolkit of the
//!   90s and most of today's do, it costs nothing, and it keeps the inner
//!   loops integer-only. Correct (linear-light) blending would need a
//!   512-entry LUT in and a 4096-entry LUT out, or f32 pixels; revisit when
//!   there is a reason.
//! - **Opaque fast path.** When the fill is opaque and `opacity == 1`, the
//!   interior of a shape is *stored*, not blended — one 4-byte write per
//!   pixel, no reads from (typically write-combined) buffer memory.
//! - **Allocation.** None. Not per call, not per frame. There is no scratch
//!   buffer either: coverage is computed on the fly from a small fixed-size
//!   [`RowSpans`](crate::shape) value that lives on the stack, so no
//!   `&mut Scratch` parameter is needed. Recursion depth is zero.
//! - **`unsafe`.** None, in a crate that is nothing but pixel loops. The
//!   loops slice a row once and then run `chunks_exact_mut(4)`, which the
//!   compiler autovectorizes; there is no explicit SIMD and nothing
//!   target-feature-gated, so the same binary runs on the SSE4.2-only test
//!   box.
//!
//! # Limitations (M1)
//!
//! - No rotation or shear — axis-aligned rects only.
//! - No gamma-correct blending (see above).
//! - Linear gradients are axis-aligned: a gradient whose axis is diagonal is
//!   projected onto its dominant component. The scene never asks for one.
//! - No text; glyph blitting from the server-side atlas arrives with the text
//!   work and will reuse [`Canvas::blit`]'s coverage path.
//! - No radial/sweep gradients, no blur, no blend modes other than
//!   source-over.

#![forbid(unsafe_code)]

mod blend;
mod canvas;
mod paint;
mod shape;

pub use canvas::{BYTES_PER_PIXEL, Canvas, Fill, Image, PixelFormat};

#[cfg(test)]
mod tests;
