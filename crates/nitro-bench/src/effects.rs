//! The demo effects: pure pixel arithmetic, no socket, no clock.
//!
//! # Why the effects live here and not in the scenarios
//!
//! The benchmark's subject is the *wire* — how long it takes a client to
//! get a frame of pixels, or a moved node, in front of the user. An effect
//! that touched the connection, read the clock, or allocated per frame
//! would put its own cost inside the number being reported, and worse,
//! would make the number irreproducible. So everything in this module is
//! a pure function from (construction parameters, frame index, surface
//! size) to bytes, and the only output is a [`Surface`] the caller owns.
//! That is what makes a checksum a legitimate test: two runs of the same
//! frame must produce the same bits on any machine.
//!
//! # Why these effects
//!
//! x11perf measures primitives; primitives do not tell you whether a
//! desktop feels alive. The classic demos do, and they do it with a known
//! shape of load: plasma and rotozoom are *full-surface rewrites* (every
//! pixel every frame — the worst case for a shared-memory upload), fire
//! is a full rewrite with a serial dependency between rows, and boing,
//! starfield and balls are *sparse* — a handful of small moving things,
//! which is exactly the load a scene graph is supposed to win. Running
//! the sparse three both as pixels and as nodes is the comparison the
//! whole crate exists to make, which is why [`Boing`], [`Starfield`] and
//! [`Balls`] expose their simulation as well as their pixels.
//!
//! # Determinism, stated precisely
//!
//! [`Plasma`], [`Rotozoom`] and [`Boing`] are pure in the strong sense:
//! frame *n* is computed from *n* alone, so rendering frame 3 on a fresh
//! instance is byte-identical to rendering 0, 1, 2, 3.
//!
//! [`Fire`], [`Starfield`] and [`Balls`] are simulations and are honest
//! about it — a cellular automaton and two integrators. Their guarantee
//! is weaker but still strong enough to test and to benchmark: **a fresh
//! instance advanced from frame 0 in order produces the same bytes every
//! time**, because the PRNG is seeded from a constant and nothing
//! consults the clock. They catch up on a forward skip, but cannot be
//! rewound: asked for a frame already passed, they redraw where they
//! are, because a cooling step has no inverse and pretending otherwise
//! would be a lie in the API. Both the tests and the benchmark runner
//! drive them forward from 0, so a checksum regression still means a
//! real change.
//!
//! # No dependencies
//!
//! Nothing here uses a crate. The sine table, the PRNG and the palettes
//! are ten lines each, and writing them down is cheaper than owning a
//! dependency in a benchmark whose results are supposed to be comparable
//! across years.

/// A client-side BGRA pixel buffer, laid out the way the wire wants it.
///
/// The format is `AR24`/`XR24`: little-endian 32-bit words whose bytes in
/// memory are `[b, g, r, a]`. That is not a taste decision — it is what a
/// nitro client buffer is, so an effect that wrote RGBA would need a
/// conversion pass between the effect and the upload, and the benchmark
/// would be measuring the conversion.
///
/// `stride` is stored explicitly even though [`Surface::new`] always sets
/// it to `width * 4`. A real client buffer's stride comes from the
/// server's allocator and may be padded; keeping the field means the
/// blitting code here is already the code that would work against a
/// padded buffer, instead of code that would have to be rewritten the
/// first time it met one.
pub struct Surface {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bytes between the start of one row and the start of the next.
    pub stride: u32,
    /// The pixels: `stride * height` bytes, `[b, g, r, a]` per pixel.
    pub data: Vec<u8>,
}

impl Surface {
    /// A black, fully opaque surface of `width` × `height`.
    ///
    /// Zero-filled, which in BGRA is transparent black. Effects that want
    /// an opaque backdrop fill it themselves; the ones that do not (the
    /// boing sprite) want the zeroes, so zero is the useful default and
    /// `vec![0; n]` is a `calloc` rather than a memset.
    pub fn new(width: u32, height: u32) -> Self {
        let stride = width * 4;
        let data = vec![0u8; (stride as usize) * (height as usize)];
        Self {
            width,
            height,
            stride,
            data,
        }
    }

    /// FNV-1a over every byte of `data`.
    ///
    /// The tests assert on this, so it must be exactly reproducible:
    /// FNV-1a has no table, no seed and no endianness to get wrong. It is
    /// not a security hash and does not need to be — the adversary is a
    /// refactor, not an attacker, and a one-pixel change flips it.
    ///
    /// Padding bytes between rows are included. For surfaces made here
    /// there are none, but a checksum that skipped padding would hide
    /// exactly the uninitialised-memory bug worth catching.
    pub fn checksum(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in &self.data {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    /// Size of the pixel data in bytes — what an upload would have to move.
    ///
    /// Reported next to the frame time, because "12 ms per frame" means
    /// nothing until you know whether it moved 300 KiB or 8 MiB.
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// Fill every pixel with `bgra`.
    ///
    /// Written as a chunk loop over `to_le_bytes` rather than anything
    /// clever: the optimiser turns it into a wide store, and a clever
    /// version would be the first thing to go wrong on a padded stride.
    pub fn fill(&mut self, bgra: u32) {
        let px = bgra.to_le_bytes();
        for chunk in self.data.chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
    }

    /// Write one pixel, silently ignoring coordinates off the surface.
    ///
    /// Clipping here rather than at every call site is deliberate. The
    /// effects are full of things that legitimately wander off the edge —
    /// a star at the screen boundary, a ball mid-bounce, the corner of a
    /// rotated texture — and making each of them prove it is in bounds
    /// would add a branch per call anyway while giving six chances to get
    /// the comparison wrong. Signed coordinates for the same reason: `-1`
    /// must be *outside*, not `u32::MAX`.
    pub fn put(&mut self, x: i32, y: i32, bgra: u32) {
        if x < 0 || y < 0 || x >= signed(self.width) || y >= signed(self.height) {
            return;
        }
        let off = (y as usize) * (self.stride as usize) + (x as usize) * 4;
        self.data[off..off + 4].copy_from_slice(&bgra.to_le_bytes());
    }
}

/// What every effect implements: advance to absolute frame `frame` and draw it.
///
/// The frame index is passed in rather than kept inside the effect so the
/// benchmark runner owns the timeline. A runner that drops frames, or
/// that renders the same frame twice to separate compute cost from upload
/// cost, stays honest — for the pure effects it gets the same pixels, and
/// for the simulations it gets the documented "fresh instance, in order"
/// guarantee, which the runner satisfies.
pub trait Effect {
    /// Short stable name, used in the report's rows.
    fn name(&self) -> &'static str;

    /// Render frame `frame` into `surface`.
    ///
    /// Must be a pure function of (the effect's construction parameters,
    /// `frame`, and the surface size) so that a checksum is stable — with
    /// the documented exception of the stateful three, for which it must
    /// be a pure function of that plus the frames already rendered.
    fn render(&mut self, surface: &mut Surface, frame: u64);
}

// ---------------------------------------------------------------------------
// Shared arithmetic: a sine table and a PRNG, both written out on purpose.
// ---------------------------------------------------------------------------

/// Number of entries in the sine table; a power of two so the index wraps
/// with a mask instead of a modulo.
///
/// Signed, because every consumer is doing phase arithmetic on sums of
/// pixel coordinates that legitimately go negative, and a `usize` length
/// would put a cast at each of those sites instead of one here.
const SIN_LEN: i32 = 1024;

/// Mask that turns any non-negative phase into a table index.
const SIN_MASK: usize = (SIN_LEN - 1) as usize;

/// One period of a sine, sampled at [`SIN_LEN`] points.
///
/// A table rather than [`f32::sin`], with one honest caveat. A 1990s
/// plasma *was* a table lookup, so a table is what makes the per-pixel
/// cost period-correct, and it costs the same on every machine, which
/// keeps the numbers comparable across the hardware this will be run on.
/// The caveat: libm's `sin` is not the bottleneck here — the upload is —
/// so nobody should read this as the table having bought a measurable
/// win. It bought *predictability*, which is what a benchmark needs.
///
/// Built by the caller and kept, rather than being a `static`: a
/// `const fn` cannot call `sin`, and a lazily-initialised static would
/// need synchronisation for no benefit at 1024 entries.
fn sin_table() -> Vec<f32> {
    let mut table = Vec::with_capacity(SIN_LEN as usize);
    for i in 0..SIN_LEN {
        let angle = (i as f32) * (core::f32::consts::TAU / SIN_LEN as f32);
        table.push(angle.sin());
    }
    table
}

/// Look up `sin` for a phase in table units, wrapping.
///
/// Takes an `i32` because every caller computes a phase by summing scaled
/// coordinates, which goes negative on the left of a centred effect; the
/// mask on a `usize` cast of a negative number would be wrong, so the
/// wrap is done in signed arithmetic first and the mask is belt to that
/// braces.
fn tsin(table: &[f32], phase: i32) -> f32 {
    let idx = phase.rem_euclid(SIN_LEN) as usize;
    table[idx & SIN_MASK]
}

/// A pixel count as a signed coordinate.
///
/// One function with one allow, rather than a scattering of `as i32`.
/// Every coordinate in this module is signed — [`Surface::put`] needs
/// `-1` to mean "off the left edge", not `u32::MAX` — while sizes arrive
/// as `u32` from the surface, so the conversion happens constantly. It
/// cannot wrap for any surface that can be allocated: 2³¹ pixels on a
/// side is 8 TiB of a single row.
#[allow(clippy::cast_possible_wrap)]
fn signed(v: u32) -> i32 {
    v as i32
}

/// `xorshift64*`, the whole of the randomness in this module.
///
/// Fire needs a few hundred thousand random bytes a second and the
/// starfield needs a respawn position; neither needs statistical quality
/// beyond "does not visibly repeat". `xorshift64*` passes the practical
/// subset of `BigCrush`, is three shifts and a multiply, and — the part
/// that matters here — is *ours*, so the frames it produces cannot change
/// underneath the checksums when a dependency bumps its algorithm. That
/// has happened to benchmarks before; it is the reason `rand` is not in
/// this crate's dependency list.
struct Rng(u64);

impl Rng {
    /// Seed the generator, forcing a non-zero state.
    ///
    /// Zero is xorshift's fixed point: it would produce an all-black fire
    /// that still passed a naive "it renders" test, so it is mapped to a
    /// constant instead of being trusted.
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9e37_79b9_7f4a_7c15
        } else {
            seed
        })
    }

    /// Next 64 bits.
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Next value in `0..n`, for small `n`.
    ///
    /// Plain modulo. The bias is `n / 2^64`, which for the `n ≤ 4` this
    /// module uses is unmeasurable, and rejection sampling would add a
    /// branch to the fire's innermost loop to fix a bias no eye can see.
    fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % u64::from(n)) as u32
    }

    /// Next `f32` in `0.0..1.0`.
    ///
    /// Built from the top 24 bits, which are the well-mixed ones in a
    /// multiply-finalised generator; taking the low bits of an xorshift
    /// is the classic way to get a visibly periodic starfield.
    fn next_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / ((1u32 << 24) as f32)
    }
}

/// Pack `(r, g, b)` into an opaque BGRA word.
///
/// The one place in this module that knows the byte order, so that the
/// palettes below read as colours rather than as shifts.
const fn bgra(r: u8, g: u8, b: u8) -> u32 {
    0xff00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

// ---------------------------------------------------------------------------
// Palettes
// ---------------------------------------------------------------------------

/// Palette for the plasma: 256 entries of BGRA, a smooth three-phase ramp.
///
/// A `const fn` so the table is materialised at compile time and costs
/// the benchmark nothing: a plasma that spent its first frame building a
/// palette would show a warm-up artefact in the first sample, which is
/// the sample people quote.
///
/// The ramp is three triangle waves at 120° apart rather than an HSV
/// sweep, because a triangle wave is exact in integer arithmetic (so the
/// table is `const`-computable) and because the resulting cyan/magenta
/// smear is what a plasma actually looked like.
pub const fn plasma_palette() -> [u32; 256] {
    let mut palette = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        // Triangle wave of period 256 at three phase offsets. The `u8`
        // wrapping add is the phase shift; the fold makes it a triangle.
        let step = i as u8;
        let red = tri(step);
        let green = tri(step.wrapping_add(85));
        let blue = tri(step.wrapping_add(170));
        palette[i as usize] = bgra(red, green, blue);
        i += 1;
    }
    palette
}

/// A 0..=255 triangle wave of `x`, used by [`plasma_palette`].
const fn tri(x: u8) -> u8 {
    if x < 128 { x * 2 } else { (255 - x) * 2 + 1 }
}

/// Doom-style fire palette: black → red → orange → yellow → white.
///
/// The four-segment ramp every fire effect since the 1993 Doom PSX port
/// has used, and it is not arbitrary: heat maps to colour the way a black
/// body does, so red comes up first, green follows to make orange and
/// then yellow, and blue only joins at the top. A linear grey ramp would
/// be physically wrong and would read as smoke rather than flame.
///
/// A `fn` rather than a `const fn` only because four interpolated
/// segments are clearer written this way; it is called once in
/// [`Fire::new`] and never in a frame loop.
pub fn fire_palette() -> [u32; 256] {
    let mut palette = [0u32; 256];
    for (i, slot) in palette.iter_mut().enumerate() {
        let heat = i as u32;
        let (red, green, blue) = match heat {
            // Black to red over the first quarter: embers.
            0..=63 => (heat * 4, 0, 0),
            // Red to orange/yellow: green climbs while red stays pinned.
            64..=127 => (255, (heat - 64) * 4, 0),
            // Yellow holds while blue starts: the hottest visible flame.
            128..=191 => (255, 255, (heat - 128) * 4),
            // The top of the range is white; the seed row lives here.
            _ => (255, 255, 255),
        };
        *slot = bgra(red as u8, green as u8, blue as u8);
    }
    palette
}

/// The bouncing-balls colours.
///
/// Eight saturated hues, one per ball modulo the count, chosen to stay
/// distinguishable against the dark backdrop *and* against each other
/// when two balls overlap — the node variant draws them as opaque rounded
/// rects, so an overlap has to read as two objects or the comparison
/// between the pixel and the node path is not visually checkable.
pub const BALL_COLORS: [u32; 8] = [
    bgra(0xff, 0x40, 0x40),
    bgra(0x40, 0xff, 0x60),
    bgra(0x50, 0x90, 0xff),
    bgra(0xff, 0xd0, 0x40),
    bgra(0xff, 0x60, 0xd0),
    bgra(0x40, 0xe0, 0xe0),
    bgra(0xff, 0xa0, 0x50),
    bgra(0xc0, 0xc0, 0xd0),
];

/// Colours the x11perf-style rect/move scenarios cycle through.
///
/// Cycling rather than using one colour is not decoration: a constant
/// colour lets a compositor notice that a rect is unchanged and skip it,
/// and then the benchmark measures the skip instead of the fill. Eight is
/// enough that consecutive rects always differ and few enough that the
/// table is obviously not the thing being measured.
pub const BENCH_COLORS: [u32; 8] = [
    bgra(0xe0, 0x30, 0x30),
    bgra(0x30, 0xe0, 0x30),
    bgra(0x30, 0x60, 0xe0),
    bgra(0xe0, 0xe0, 0x30),
    bgra(0xe0, 0x30, 0xe0),
    bgra(0x30, 0xe0, 0xe0),
    bgra(0xf0, 0x90, 0x20),
    bgra(0xf0, 0xf0, 0xf0),
];

// ---------------------------------------------------------------------------
// Plasma
// ---------------------------------------------------------------------------

/// Sine-sum plasma — the demoscene's hello-world, everywhere from 1990.
///
/// Four sine terms (x, y, x+y, and a radial distance from a point that
/// itself moves) summed into one index, looked up in an animated palette.
/// The animation is a palette *rotation*, exactly as it was done when the
/// hardware had a real CLUT and rotating it was free: here it costs an
/// add per pixel, but keeping the structure means the effect's shape of
/// work — one table lookup and a handful of adds per pixel — matches the
/// original, which is what makes it a fair stand-in for "a client that
/// rewrites every pixel every frame".
///
/// Stateless: frame *n* depends only on *n*, so the checksum for a frame
/// is the same however you got there.
pub struct Plasma {
    /// The sine lookup table, built once in [`Plasma::new`].
    table: Vec<f32>,
    /// The 256-entry colour ramp, rotated by frame at lookup time.
    palette: [u32; 256],
}

impl Default for Plasma {
    fn default() -> Self {
        Self::new()
    }
}

impl Plasma {
    /// A plasma with its table and palette built.
    ///
    /// Both allocations happen here and never again: the point of the
    /// benchmark is the wire, so an effect that allocated inside `render`
    /// would be charging the allocator to the transport.
    pub fn new() -> Self {
        Self {
            table: sin_table(),
            palette: plasma_palette(),
        }
    }
}

impl Effect for Plasma {
    fn name(&self) -> &'static str {
        "plasma"
    }

    fn render(&mut self, surface: &mut Surface, frame: u64) {
        let width = signed(surface.width);
        let height = signed(surface.height);
        // Phase is taken modulo the table length before it can grow: at
        // 60 Hz a `u64` frame counter would not overflow an `i32` for a
        // year, but "would not overflow for a year" is not an argument a
        // benchmark that might be run in a loop should rely on.
        let t = (frame % SIN_LEN as u64) as i32;
        let cx = width / 2 + (tsin(&self.table, t * 3) * (width as f32) * 0.25) as i32;
        let cy = height / 2 + (tsin(&self.table, t * 2 + 256) * (height as f32) * 0.25) as i32;

        for y in 0..height {
            for x in 0..width {
                let dx = x - cx;
                let dy = y - cy;
                // Integer distance: a square root per pixel would be the
                // one genuinely expensive operation in the loop, and the
                // original effects used a distance table for exactly that
                // reason. `isqrt` is the modern spelling of that table,
                // and it is exact, so the checksums do not depend on how
                // a given target rounds `sqrt`.
                let radius = signed((dx * dx + dy * dy) as u32).isqrt();
                let sum = tsin(&self.table, x * 7 + t * 4)
                    + tsin(&self.table, y * 5 - t * 3)
                    + tsin(&self.table, (x + y) * 4 + t * 2)
                    + tsin(&self.table, radius * 6 - t * 5);
                // Four sines are in -4.0..=4.0; map to 0..=255.
                let idx = (((sum + 4.0) * 31.9) as i32).clamp(0, 255) as usize;
                // The palette rotation *is* the animation, exactly as it
                // was when rotating a hardware CLUT was free.
                surface.put(x, y, self.palette[(idx + (frame as usize)) & 0xff]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fire
// ---------------------------------------------------------------------------

/// The classic cellular fire, as in the 1997 `PlayStation` port of Doom.
///
/// The algorithm is four lines: seed the bottom row at full heat, and for
/// every other cell take the cell below, subtract a small random amount,
/// and write the result one row up with a random horizontal jitter. The
/// jitter is what makes the flame lean and flicker; without it the fire
/// is a set of vertical stripes.
///
/// # Why an internal `u8` grid
///
/// Heat is kept as one byte per cell, separate from the BGRA surface, and
/// only converted to colour when a frame is drawn. That is how the
/// original worked (the grid *was* palette indices) and it also keeps the
/// simulation's cost — one byte read, one subtract, one byte write —
/// distinct from the colour conversion, so a profile of the benchmark can
/// tell the two apart.
///
/// # Determinism
///
/// Stateful on purpose, with the contract in the module docs: reproducible
/// for a fresh instance driven from frame 0 in order. The tests assert the
/// state showing through — a fire at frame 10 asked for frame 3 redraws
/// frame 10, and a differently-seeded fire is a different picture — as
/// properties, so that nobody later "fixes" this into pretending to be
/// pure.
pub struct Fire {
    /// Grid width in cells; one cell per surface pixel.
    width: u32,
    /// Grid height in cells.
    height: u32,
    /// Heat per cell, row-major, `0` cold and `255` white-hot.
    heat: Vec<u8>,
    /// Heat-to-colour ramp.
    palette: [u32; 256],
    /// The cooling/jitter source, seeded from a constant.
    rng: Rng,
    /// Last frame simulated, so a repeated `render` of the same frame
    /// redraws rather than advancing the automaton twice.
    last: Option<u64>,
}

impl Fire {
    /// A cold grid of `width` × `height` with the bottom row seeded hot.
    ///
    /// The seed row is planted at construction rather than on the first
    /// frame so that frame 0 already shows flame: a benchmark whose first
    /// few frames are black would be reporting the cost of drawing black.
    pub fn new(width: u32, height: u32) -> Self {
        // A fixed seed, not a clock: the checksums in the tests are the
        // contract, and a time-seeded fire would have none.
        Self::seeded(width, height, 0x00f1_2e3d_4c5b_6a79)
    }

    /// The same, with the PRNG seed chosen by the caller.
    ///
    /// Private, because the public shape of the effect is "a fire of this
    /// size" and a seed in that signature would invite a caller to pass a
    /// clock — which is precisely how a benchmark stops being
    /// reproducible. It exists so the tests can demonstrate that the seed
    /// genuinely drives the picture, which is the evidence that the fire
    /// is stateful rather than accidentally deterministic.
    fn seeded(width: u32, height: u32, seed: u64) -> Self {
        let mut heat = vec![0u8; (width as usize) * (height as usize)];
        if height > 0 {
            let base = ((height - 1) as usize) * (width as usize);
            for cell in &mut heat[base..] {
                *cell = 255;
            }
        }
        Self {
            width,
            height,
            heat,
            palette: fire_palette(),
            rng: Rng::new(seed),
            last: None,
        }
    }

    /// Advance the automaton by one step.
    ///
    /// Rows are walked bottom-up so each row reads the *previous* frame's
    /// row below it exactly once; walking top-down would let heat
    /// propagate the whole height in a single step and the flame would
    /// have no shape.
    fn step(&mut self) {
        let width = signed(self.width);
        for y in (1..signed(self.height)).rev() {
            for x in 0..width {
                let src = (y as usize) * (self.width as usize) + (x as usize);
                let heat = self.heat[src];
                // Three outcomes: two of cooling, one of lean. Cooling by
                // 0 sometimes is what lets a column survive to the top.
                let roll = self.rng.below(3);
                let decay = u8::from(roll > 0);
                let dst_x = (x + signed(roll) - 1).rem_euclid(width);
                let dst = ((y - 1) as usize) * (self.width as usize) + (dst_x as usize);
                self.heat[dst] = heat.saturating_sub(decay);
            }
        }
    }

    /// Paint the current heat grid into `surface`.
    ///
    /// Clipped to the smaller of the grid and the surface so that a
    /// caller who resized the surface without rebuilding the fire gets a
    /// cropped flame rather than a panic: the grid's size is a
    /// construction parameter and the surface's is not, so they can
    /// legitimately disagree.
    fn blit(&self, surface: &mut Surface) {
        let width = signed(surface.width.min(self.width));
        let height = signed(surface.height.min(self.height));
        for y in 0..height {
            for x in 0..width {
                let heat = self.heat[(y as usize) * (self.width as usize) + (x as usize)];
                surface.put(x, y, self.palette[heat as usize]);
            }
        }
    }
}

impl Effect for Fire {
    fn name(&self) -> &'static str {
        "fire"
    }

    /// Simulate forward to `frame` and draw.
    ///
    /// Re-rendering the frame just drawn is a redraw, not a second step,
    /// so a runner that separates compute cost from upload cost by
    /// rendering the same frame twice gets the same pixels both times.
    /// Skipping ahead runs the intervening steps, because a cellular
    /// automaton has no closed form; going *backwards* is not possible
    /// and simply redraws the current state, which is the only honest
    /// thing a one-directional simulation can do.
    fn render(&mut self, surface: &mut Surface, frame: u64) {
        let from = self.last.map_or(0, |l| l + 1);
        if self.last.is_none() || frame >= from {
            for _ in from..=frame {
                self.step();
            }
            self.last = Some(frame);
        }
        self.blit(surface);
    }
}

// ---------------------------------------------------------------------------
// Rotozoom
// ---------------------------------------------------------------------------

/// Edge length of the rotozoom's procedural texture.
///
/// 64 so that wrapping is a mask and the whole texture (16 KiB) sits in
/// L1: the effect is meant to be bound by the per-pixel affine step, not
/// by cache misses on a texture nobody chose the size of.
const ROTO_EDGE: i32 = 64;

/// Rotate-and-zoom texture mapping, the Amiga/PC demoscene's calling card.
///
/// Every output pixel is mapped back into a small tiling texture through
/// a rotation and a scale, and sampled nearest-neighbour. Because the
/// transform is affine, the texture coordinate of the next pixel on a
/// row is the current one plus a constant — so the inner loop is two
/// adds and a masked lookup, and *that* is the effect people wrote in
/// 1992 on a machine with no multiplier worth the name.
///
/// # Why fixed point
///
/// The texture coordinates are `i32` in 16.16 fixed point. It is
/// period-correct, and it is what makes the "two adds per pixel" claim
/// above true rather than a story about what floats might compile to.
/// The high bits also wrap into the texture with a mask, so tiling costs
/// no per-pixel branch; floats would need a `rem_euclid` or a clamp each
/// time and would round differently on different targets, which would
/// cost the checksums their stability.
///
/// Stateless: frame *n* depends only on *n*.
pub struct Rotozoom {
    /// The procedural texture, `ROTO_EDGE` × `ROTO_EDGE` BGRA words.
    texture: Vec<u32>,
    /// Sine table, shared by the rotation and the zoom pulse.
    table: Vec<f32>,
}

impl Default for Rotozoom {
    fn default() -> Self {
        Self::new()
    }
}

impl Rotozoom {
    /// A rotozoom over a texture it generates itself.
    ///
    /// The texture is an XOR pattern (`x ^ y`, the other demoscene
    /// hello-world) tinted through a colour ramp. Generated rather than
    /// loaded so that the crate has no asset to ship, no file to read at
    /// benchmark time, and no way for a missing file to change what is
    /// measured. The XOR pattern is also the ideal test image here: it is
    /// high-frequency in both axes, so a sampling bug shows up as visible
    /// moiré instead of hiding in a smooth gradient.
    pub fn new() -> Self {
        let n = (ROTO_EDGE * ROTO_EDGE) as usize;
        let mut texture = Vec::with_capacity(n);
        for y in 0..ROTO_EDGE {
            for x in 0..ROTO_EDGE {
                let v = ((x ^ y) * 4) as u8;
                // A blue/amber ramp: two channels rise with the XOR value
                // and one falls, so neighbouring texels differ in hue as
                // well as in brightness and the rotation stays legible
                // even at small zoom.
                texture.push(bgra(v, v / 2 + 40, 255 - v));
            }
        }
        Self {
            texture,
            table: sin_table(),
        }
    }

    /// Sample the texture with wrapping, given 16.16 coordinates.
    ///
    /// The wrap is `>> 16` then mask, which is why [`ROTO_EDGE`] is a
    /// power of two. Negative coordinates work because the shift is
    /// arithmetic and the mask is applied to the result as a `usize`
    /// *after* taking the remainder in signed space.
    fn sample(&self, u: i32, v: i32) -> u32 {
        let tx = ((u >> 16).rem_euclid(ROTO_EDGE)) as usize;
        let ty = ((v >> 16).rem_euclid(ROTO_EDGE)) as usize;
        self.texture[ty * (ROTO_EDGE as usize) + tx]
    }
}

impl Effect for Rotozoom {
    fn name(&self) -> &'static str {
        "rotozoom"
    }

    fn render(&mut self, surface: &mut Surface, frame: u64) {
        let width = signed(surface.width);
        let height = signed(surface.height);
        let t = (frame % SIN_LEN as u64) as i32;

        // Rotation angle and a zoom that breathes between roughly 0.5×
        // and 2×. Both come from the same table the plasma uses, so the
        // module has exactly one source of trigonometry.
        let angle = t * 3;
        let zoom = 1.25 + tsin(&self.table, t * 2) * 0.75;
        let cos = tsin(&self.table, angle + SIN_LEN / 4) * zoom;
        let sin = tsin(&self.table, angle) * zoom;

        // Column and row steps in 16.16. Computed once per frame: this is
        // the whole trick, and the reason the inner loop has no multiply.
        let col_u = (cos * 65536.0) as i32;
        let col_v = (sin * 65536.0) as i32;
        let row_u = (-sin * 65536.0) as i32;
        let row_v = (cos * 65536.0) as i32;

        // Start the walk so the surface centre maps to the texture
        // centre; otherwise the rotation orbits a corner, which looks
        // like a bug and makes the sampling pattern less uniform. The
        // arithmetic wraps by design — the low 16 bits are the fraction
        // and the high bits are taken modulo the texture at sample time,
        // so an overflow on a very wide surface is not an error.
        let mut u_start = ((ROTO_EDGE / 2) << 16)
            .wrapping_sub(col_u.wrapping_mul(width / 2))
            .wrapping_sub(row_u.wrapping_mul(height / 2));
        let mut v_start = ((ROTO_EDGE / 2) << 16)
            .wrapping_sub(col_v.wrapping_mul(width / 2))
            .wrapping_sub(row_v.wrapping_mul(height / 2));

        for y in 0..height {
            let mut u = u_start;
            let mut v = v_start;
            for x in 0..width {
                surface.put(x, y, self.sample(u, v));
                u = u.wrapping_add(col_u);
                v = v.wrapping_add(col_v);
            }
            u_start = u_start.wrapping_add(row_u);
            v_start = v_start.wrapping_add(row_v);
        }
    }
}

// ---------------------------------------------------------------------------
// Boing
// ---------------------------------------------------------------------------

/// The Amiga Boing Ball, Dale Luck and R. J. Mical, CES 1984.
///
/// A red-and-white chequered sphere spinning about a tilted axis,
/// bouncing off the walls and falling under gravity, in front of a purple
/// grid. It was written overnight for a show floor and ended up being the
/// thing everyone remembers about the machine — which is a fair
/// description of what a compositor benchmark is after: not throughput,
/// but whether one small moving object stays smooth.
///
/// # Why it is here twice
///
/// The ball is a small opaque sprite on a static background. As pixels
/// that means re-uploading the whole surface every frame; as a scene node
/// it means uploading the sprite once and then sending a position. The
/// gap between those two numbers is the single most useful result this
/// crate produces, so [`Boing::position`] and [`Boing::sprite`] expose
/// the same motion the pixel path uses — driving the node variant from a
/// *different* simulation would make it a comparison of two programs
/// rather than of two paths.
///
/// The motion is a closed form of the frame index, so despite looking
/// like a simulation this effect is stateless in the strong sense: the
/// bounce is a triangle wave and gravity is folded into it by shaping,
/// so nothing has to be integrated and nothing can drift.
pub struct Boing {
    /// Surface width the motion is scaled to.
    width: u32,
    /// Surface height the motion is scaled to.
    height: u32,
    /// Sine table, for the shading falloff and the axis tilt.
    table: Vec<f32>,
}

impl Boing {
    /// A boing ball sized and bounded for a `width` × `height` surface.
    ///
    /// The box is taken at construction rather than read from the surface
    /// in `render` so that [`Boing::position`] — which has no surface —
    /// answers in the same coordinates. A node-path caller that asked for
    /// positions in one box and rendered into another would get a ball
    /// that bounced off invisible walls.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            table: sin_table(),
        }
    }

    /// Ball centre and radius in surface pixels at `frame`.
    ///
    /// Horizontal motion is a triangle wave — constant speed, hard
    /// reversal at the wall, exactly like the original. Vertical motion
    /// is a *rectified* shape: the ball falls, hits the floor and comes
    /// back up with the same energy, which is what the demo did (it never
    /// lost height) and which has the nice property of being periodic, so
    /// frame *n* needs no integration and cannot drift.
    pub fn position(&self, frame: u64) -> (f32, f32, f32) {
        let box_w = self.width as f32;
        let box_h = self.height as f32;
        let radius = (box_w.min(box_h) * 0.18).max(4.0);

        // Triangle wave in 0..=1 with period 220 frames horizontally and
        // 150 vertically; the two periods share no small factor, so the
        // path does not close quickly and the eye keeps finding it new.
        let across = triangle(frame, 220);
        let down = triangle(frame, 150);

        let x = radius + across * (box_w - 2.0 * radius).max(0.0);
        // The vertical triangle is squared, which turns a linear
        // up-and-down into something that reads as gravity without
        // integrating an acceleration: slow near the apex, fastest at the
        // floor. `t^2` is the cheapest curve with that shape, and being a
        // closed form it cannot drift the way an integrator would over a
        // benchmark's worth of frames.
        let y = radius + down * down * (box_h - 2.0 * radius).max(0.0);
        (x, y, radius)
    }

    /// The pre-rendered sprite: a square BGRA [`Surface`] of the ball.
    ///
    /// `edge` is the side of the square (the ball fills it) and
    /// `frame_phase` selects the rotation, so a caller that wants the
    /// spin can upload a handful of phases and cycle them, or upload one
    /// and accept a ball that slides without turning — the point of the
    /// node path is that the *upload* happens rarely.
    ///
    /// Outside the circle the alpha is 0 and the colour is 0, so the
    /// sprite can go up as an `AR24` buffer and be composited over
    /// whatever the server has: a sprite with an opaque square background
    /// would make the node path look wrong on any backdrop but the one it
    /// was baked against, and would quietly measure a larger blend.
    pub fn sprite(&self, edge: u32, frame_phase: u32) -> Surface {
        let mut sprite = Surface::new(edge, edge);
        if edge == 0 {
            return sprite;
        }
        let radius = (edge as f32) / 2.0;
        for y in 0..signed(edge) {
            for x in 0..signed(edge) {
                // Sampled at pixel centres, not corners: sampling at the
                // corner biases the disc up and left by half a pixel,
                // which at a 16-px sprite is a visibly lopsided edge.
                let dx = (x as f32) + 0.5 - radius;
                let dy = (y as f32) + 0.5 - radius;
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                sprite.put(x, y, self.shade(dx / radius, dy / radius, frame_phase));
            }
        }
        sprite
    }

    /// Colour of the sphere at normalised offset `(nx, ny)` from its
    /// centre, for rotation phase `phase`.
    ///
    /// The chequer is computed in *sphere* coordinates, not screen ones:
    /// the third coordinate is recovered as `sqrt(1 - nx² - ny²)` and
    /// latitude and longitude come from that. Doing it in screen space
    /// gives a flat chequered disc — the classic way to get this effect
    /// wrong, and the reason the original looks spherical at all.
    ///
    /// The tilt is a rotation of the sphere coordinates about the
    /// screen's x axis, taken before the longitude, because the Boing
    /// ball's axis leans back; a vertical axis reads as a rolling barrel
    /// rather than a spinning ball.
    fn shade(&self, nx: f32, ny: f32, phase: u32) -> u32 {
        let nz2 = 1.0 - nx * nx - ny * ny;
        let nz = if nz2 > 0.0 { nz2.sqrt() } else { 0.0 };

        // Tilt: rotate (y, z) by a fixed ~17°, the lean of the original.
        let (ct, st) = (0.956, 0.292);
        let ty = ny * ct - nz * st;
        let tz = ny * st + nz * ct;

        // Latitude from the tilted y, longitude from x and the tilted z,
        // both quantised into bands. Eight longitude bands and four
        // latitude bands is the original's count.
        let lat = (ty.clamp(-1.0, 1.0).asin() * (4.0 / core::f32::consts::FRAC_PI_2)) as i32;
        let spin = (phase as f32) * 0.06;
        let lon = ((tz.atan2(nx) + spin) * (8.0 / core::f32::consts::PI)) as i32;
        let dark = (lat + lon).rem_euclid(2) == 0;

        // Shading falloff: a Lambert-ish term from a light up and to the
        // left, floored so the dark side is still coloured rather than
        // black. Without it the disc is flat no matter how good the
        // chequer is — brightness, not the pattern, is what the eye reads
        // as roundness.
        let lightness = (nx * -0.45 + ny * -0.45 + nz * 0.85).clamp(0.0, 1.0);
        let k = 0.35 + 0.65 * lightness;
        // A faint table-driven ripple keeps the sine table honest about
        // being used by this effect too, and mimics the original's
        // banding on a 12-bit palette.
        let k = k * (0.98 + 0.02 * tsin(&self.table, (nz * 200.0) as i32));

        if dark {
            bgra((230.0 * k) as u8, (30.0 * k) as u8, (40.0 * k) as u8)
        } else {
            bgra((245.0 * k) as u8, (245.0 * k) as u8, (245.0 * k) as u8)
        }
    }
}

impl Effect for Boing {
    fn name(&self) -> &'static str {
        "boing"
    }

    /// Draw the backdrop grid and the ball.
    ///
    /// The grid is drawn because it is cheap (two strides of line writes)
    /// and because it does real work for the benchmark: a flat backdrop
    /// compresses and uploads unrealistically well, and a moving ball
    /// over a textured background is the case where a damage-tracking
    /// compositor has to actually repaint what the ball uncovered.
    fn render(&mut self, surface: &mut Surface, frame: u64) {
        surface.fill(bgra(0x20, 0x10, 0x30));
        let width = signed(surface.width);
        let height = signed(surface.height);
        let grid = bgra(0x55, 0x28, 0x70);
        // A 16-px pitch: fine enough that the ball always covers several
        // cells (so the uncovered region is never trivially empty), coarse
        // enough that the grid is a small fraction of the frame's writes.
        let pitch = 16;
        let mut gx = 0;
        while gx < width {
            for y in 0..height {
                surface.put(gx, y, grid);
            }
            gx += pitch;
        }
        let mut gy = 0;
        while gy < height {
            for x in 0..width {
                surface.put(x, gy, grid);
            }
            gy += pitch;
        }

        let (cx, cy, radius) = self.position(frame);
        // The spin phase wraps at 256 so the sprite variant can cache a
        // bounded number of phases and match the pixel variant exactly.
        let phase = (frame % 256) as u32;
        let left = (cx - radius) as i32;
        let top = (cy - radius) as i32;
        let edge = (radius * 2.0) as i32;
        for sy in 0..edge {
            for sx in 0..edge {
                let dx = (sx as f32) + 0.5 - radius;
                let dy = (sy as f32) + 0.5 - radius;
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let c = self.shade(dx / radius, dy / radius, phase);
                surface.put(left + sx, top + sy, c);
            }
        }
    }
}

/// A 0..=1 triangle wave of `frame` with period `period` frames.
///
/// Shared by [`Boing`]'s two axes. A triangle rather than a sine because
/// a bouncing thing has constant speed between walls and reverses
/// instantly at them; a sine would ease into the wall, which reads as the
/// ball being made of rubber rather than the wall being hard.
fn triangle(frame: u64, period: u64) -> f32 {
    let p = period.max(2);
    let half = p / 2;
    let t = frame % p;
    if t < half {
        (t as f32) / (half as f32)
    } else {
        1.0 - ((t - half) as f32) / ((p - half) as f32)
    }
}

// ---------------------------------------------------------------------------
// Starfield
// ---------------------------------------------------------------------------

/// One star, as the node variant needs it.
///
/// Positions are `f32` in surface pixels rather than integers because the
/// node path hands them to a scene graph that takes logical coordinates
/// and does its own rounding; rounding here and then letting the server
/// round again would put a half-pixel of jitter into the node path that
/// the pixel path does not have, and the whole point is to compare them.
pub struct Star {
    /// Screen x in pixels.
    pub x: f32,
    /// Screen y in pixels.
    pub y: f32,
    /// Dot size in pixels: near stars are bigger.
    pub size: f32,
    /// Brightness, 0 (invisible) to 255 (white).
    pub brightness: u8,
}

/// Flying through a star field — the oldest 3D effect there is, and the
/// one every screensaver stole.
///
/// Stars are points with a `z` that shrinks every frame; the projection
/// is `x/z`, so a star accelerates across the screen as it approaches and
/// vanishes off the edge. When `z` gets small the star is respawned at
/// the back with a fresh random position, which is the trick that makes
/// an infinite tunnel out of a fixed-size array.
///
/// # Load shape
///
/// This is the *sparse* case: a few hundred single pixels changing on an
/// otherwise black screen. As a full-surface upload that is almost
/// entirely wasted bandwidth; as scene nodes it is a few hundred tiny
/// moves. The ratio between them is the argument for a scene graph, which
/// is why [`Starfield::stars`] exists alongside `render`.
///
/// # Determinism
///
/// A simulation, under the module's contract: reproducible for a fresh
/// instance advanced from frame 0 in order, because the PRNG is seeded
/// from a constant and the step is fixed.
pub struct Starfield {
    /// Star state: x, y in a centred unit-ish space, and depth z.
    points: Vec<(f32, f32, f32)>,
    /// Projected output, rebuilt each step so [`Starfield::stars`] can
    /// hand out a slice without re-projecting.
    out: Vec<Star>,
    /// Surface width the projection is scaled to.
    width: u32,
    /// Surface height the projection is scaled to.
    height: u32,
    /// Respawn source, seeded from a constant.
    rng: Rng,
    /// Last frame simulated.
    last: Option<u64>,
}

/// Depth at which a star is respawned at the back.
const STAR_FAR: f32 = 4.0;

/// Depth below which a star has passed the viewer and must respawn.
const STAR_NEAR: f32 = 0.15;

impl Starfield {
    /// `count` stars flying toward a `width` × `height` viewport.
    ///
    /// Initial depths are spread across the whole range rather than all
    /// starting at the back, so frame 0 already looks like a field in
    /// motion; a field that started uniform would spend its first hundred
    /// frames as a single expanding ring, and those are frames the
    /// benchmark would be timing.
    pub fn new(count: usize, width: u32, height: u32) -> Self {
        let mut rng = Rng::new(0x5ee_d574_2f1e_7c0b);
        let mut points = Vec::with_capacity(count);
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let x = rng.next_f32() * 2.0 - 1.0;
            let y = rng.next_f32() * 2.0 - 1.0;
            let z = STAR_NEAR + rng.next_f32() * (STAR_FAR - STAR_NEAR);
            points.push((x, y, z));
            out.push(Star {
                x: 0.0,
                y: 0.0,
                size: 1.0,
                brightness: 0,
            });
        }
        let mut s = Self {
            points,
            out,
            width,
            height,
            rng,
            last: None,
        };
        s.project();
        s
    }

    /// Move every star one step closer and respawn the ones that passed.
    fn step(&mut self) {
        for p in &mut self.points {
            p.2 -= 0.045;
            if p.2 <= STAR_NEAR {
                p.0 = self.rng.next_f32() * 2.0 - 1.0;
                p.1 = self.rng.next_f32() * 2.0 - 1.0;
                p.2 = STAR_FAR;
            }
        }
    }

    /// Recompute the projected [`Star`] list from the current depths.
    ///
    /// Clamped into the surface rather than culled: a star that projects
    /// off the edge is about to respawn anyway, and keeping the output
    /// length equal to `count` means the node path can own a fixed set of
    /// nodes and never create or destroy one mid-benchmark — node
    /// churn is a different measurement and mixing it in here would
    /// confuse both.
    fn project(&mut self) {
        let w = self.width as f32;
        let h = self.height as f32;
        for (p, s) in self.points.iter().zip(self.out.iter_mut()) {
            let inv = 1.0 / p.2;
            s.x = (w * 0.5 + p.0 * inv * w * 0.35).clamp(0.0, (w - 1.0).max(0.0));
            s.y = (h * 0.5 + p.1 * inv * h * 0.35).clamp(0.0, (h - 1.0).max(0.0));
            // Near stars are bigger and brighter. Both curves are linear
            // in `1/z` because that is what the projection already
            // computed; a physically-motivated falloff would cost a
            // divide per star to look the same at these depths.
            let near = ((STAR_FAR - p.2) / (STAR_FAR - STAR_NEAR)).clamp(0.0, 1.0);
            s.size = 1.0 + near * 1.5;
            s.brightness = (40.0 + near * 215.0) as u8;
        }
    }

    /// Star positions and brightness at `frame`.
    ///
    /// Takes `&mut self` and returns a borrowed slice: the caller gets
    /// the same array the renderer uses, with no copy, which matters
    /// because the node path calls this once per frame for every star and
    /// a per-frame `Vec` would charge an allocation to the scene-graph
    /// side of a comparison it is supposed to win fairly.
    pub fn stars(&mut self, frame: u64) -> &[Star] {
        self.advance(frame);
        &self.out
    }

    /// Simulate forward to `frame`, if it is ahead of where we are.
    fn advance(&mut self, frame: u64) {
        let from = self.last.map_or(0, |l| l + 1);
        if self.last.is_none() || frame >= from {
            for _ in from..=frame {
                self.step();
            }
            self.project();
            self.last = Some(frame);
        }
    }
}

impl Effect for Starfield {
    fn name(&self) -> &'static str {
        "starfield"
    }

    fn render(&mut self, surface: &mut Surface, frame: u64) {
        surface.fill(bgra(0, 0, 0));
        self.advance(frame);
        for s in &self.out {
            let v = s.brightness;
            let c = bgra(v, v, v);
            let x = s.x as i32;
            let y = s.y as i32;
            surface.put(x, y, c);
            // Near stars get a 2×2 dot. A dot rather than a streak
            // because a streak would need the previous position, and
            // then the "pure function of the frame" story would need a
            // second exception for no visual gain at these speeds.
            if s.size > 2.0 {
                surface.put(x + 1, y, c);
                surface.put(x, y + 1, c);
                surface.put(x + 1, y + 1, c);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Balls
// ---------------------------------------------------------------------------

/// One bouncing ball, as the node variant needs it.
///
/// A rect rather than a centre and a radius: the scene-graph path draws
/// these as rounded rect nodes with `corners = d/2`, which is a circle as
/// far as the rasteriser is concerned, and handing the caller the rect
/// means it cannot get the centre-to-corner conversion wrong on the one
/// path whose cost is being compared against the other.
pub struct Ball {
    /// Left edge in pixels.
    pub x: f32,
    /// Top edge in pixels.
    pub y: f32,
    /// Diameter in pixels — width and height of the rect.
    pub d: f32,
    /// Index into [`BALL_COLORS`].
    pub color: usize,
}

/// Internal ball state: centre, velocity, radius, colour.
struct Body {
    /// Centre x.
    x: f32,
    /// Centre y.
    y: f32,
    /// Velocity x, pixels per frame.
    vx: f32,
    /// Velocity y, pixels per frame.
    vy: f32,
    /// Radius in pixels.
    r: f32,
    /// Index into [`BALL_COLORS`].
    color: usize,
}

/// Downward acceleration, pixels per frame squared.
const GRAVITY: f32 = 0.35;

/// Fraction of speed kept after a wall hit.
///
/// Below 1.0 so the simulation loses energy and does not accumulate
/// floating-point gain into a ball that eventually escapes the box; above
/// 0.8 so the balls keep bouncing for the whole run. The floor bounce
/// re-injects a little energy (see [`Balls::step`]) to keep the motion
/// from dying out in a long benchmark.
const DAMPING: f32 = 0.86;

/// A box of bouncing balls: the sparse-load counterpart to the plasma.
///
/// Circles under gravity, bouncing off four walls with damping. There is
/// no ball-to-ball collision, on purpose: pairwise collision is O(n²) and
/// would make the *effect* the expensive part of a benchmark whose
/// subject is the transport. Balls passing through each other also makes
/// overlap common, which is the case worth exercising on the node path
/// (overlapping opaque nodes are what a paint-order bug shows up in).
///
/// # Determinism
///
/// A simulation, under the module's contract. Initial positions and
/// velocities come from the constant-seeded PRNG, so two
/// `Balls::new(n, w, h)` are identical.
pub struct Balls {
    /// The simulated bodies.
    bodies: Vec<Body>,
    /// Rect view, rebuilt each step for [`Balls::balls`].
    out: Vec<Ball>,
    /// Box width.
    width: u32,
    /// Box height.
    height: u32,
    /// Last frame simulated.
    last: Option<u64>,
}

impl Balls {
    /// `count` balls bouncing in a `width` × `height` box.
    ///
    /// Radii vary between a sixtieth and a twentieth of the smaller box
    /// dimension, so the set contains both the cheap case (a handful of
    /// pixels) and the case that actually costs something to blend; a
    /// uniform size would let a reader draw a conclusion about one size
    /// and believe it held for all.
    pub fn new(count: usize, width: u32, height: u32) -> Self {
        let mut rng = Rng::new(0xba11_5eed_1234_9a7c);
        let w = width as f32;
        let h = height as f32;
        let unit = w.min(h).max(8.0);
        let mut bodies = Vec::with_capacity(count);
        let mut out = Vec::with_capacity(count);
        for i in 0..count {
            let r = (unit / 60.0) + rng.next_f32() * (unit / 30.0);
            bodies.push(Body {
                x: r + rng.next_f32() * (w - 2.0 * r).max(0.0),
                y: r + rng.next_f32() * (h - 2.0 * r).max(0.0),
                vx: (rng.next_f32() * 2.0 - 1.0) * 3.0,
                vy: (rng.next_f32() * 2.0 - 1.0) * 2.0,
                r,
                color: i % BALL_COLORS.len(),
            });
            out.push(Ball {
                x: 0.0,
                y: 0.0,
                d: 0.0,
                color: i % BALL_COLORS.len(),
            });
        }
        let mut b = Self {
            bodies,
            out,
            width,
            height,
            last: None,
        };
        b.sync();
        b
    }

    /// One step of gravity, motion and wall response.
    ///
    /// The position is clamped to the wall *and* the velocity reflected,
    /// in that order. Reflecting without clamping is the classic bug that
    /// lets a fast ball tunnel through a wall and stick to it, jittering
    /// forever; clamping makes "inside the box" an invariant the tests
    /// can assert over a long run rather than a hope.
    fn step(&mut self) {
        let w = self.width as f32;
        let h = self.height as f32;
        for b in &mut self.bodies {
            b.vy += GRAVITY;
            b.x += b.vx;
            b.y += b.vy;

            if b.x - b.r < 0.0 {
                b.x = b.r;
                b.vx = b.vx.abs() * DAMPING;
            } else if b.x + b.r > w {
                b.x = w - b.r;
                b.vx = -b.vx.abs() * DAMPING;
            }
            if b.y - b.r < 0.0 {
                b.y = b.r;
                b.vy = b.vy.abs() * DAMPING;
            } else if b.y + b.r > h {
                b.y = h - b.r;
                // The floor bounce keeps a minimum speed. Physically it
                // is a cheat; without it every ball settles on the floor
                // within a few hundred frames and a long benchmark ends
                // up measuring a static picture.
                b.vy = -(b.vy.abs() * DAMPING).max(4.0);
            }
        }
    }

    /// Refresh the rect view from the bodies.
    fn sync(&mut self) {
        for (b, o) in self.bodies.iter().zip(self.out.iter_mut()) {
            o.x = b.x - b.r;
            o.y = b.y - b.r;
            o.d = b.r * 2.0;
        }
    }

    /// Ball rectangles at `frame`, for the node variant.
    ///
    /// Same borrowing argument as [`Starfield::stars`]: the node path
    /// reads this every frame and must not be charged an allocation the
    /// pixel path does not pay.
    pub fn balls(&mut self, frame: u64) -> &[Ball] {
        self.advance(frame);
        &self.out
    }

    /// Simulate forward to `frame`, if it is ahead of where we are.
    fn advance(&mut self, frame: u64) {
        let from = self.last.map_or(0, |l| l + 1);
        if self.last.is_none() || frame >= from {
            for _ in from..=frame {
                self.step();
            }
            self.sync();
            self.last = Some(frame);
        }
    }
}

impl Effect for Balls {
    fn name(&self) -> &'static str {
        "balls"
    }

    /// Draw filled circles over a dark backdrop.
    ///
    /// Circles are rasterised by span: for each row of the bounding box
    /// the half-width is `sqrt(r² - dy²)` and the row is a run of
    /// identical pixels. One square root per row instead of a distance
    /// test per pixel, which matters because at 200 balls the per-pixel
    /// version becomes a visible fraction of a frame that is supposed to
    /// be dominated by the upload.
    ///
    /// Hard edges, no antialiasing: the node path draws through the
    /// server's rasteriser, and comparing an antialiased client circle
    /// against a server-antialiased rounded rect would make the two paths
    /// differ in output as well as in cost.
    fn render(&mut self, surface: &mut Surface, frame: u64) {
        surface.fill(bgra(0x10, 0x12, 0x18));
        self.advance(frame);
        for b in &self.bodies {
            let c = BALL_COLORS[b.color];
            let r = b.r;
            let cy = b.y;
            let cx = b.x;
            let y0 = (cy - r).floor() as i32;
            let y1 = (cy + r).ceil() as i32;
            for y in y0..=y1 {
                let dy = (y as f32) + 0.5 - cy;
                let inside = r * r - dy * dy;
                if inside <= 0.0 {
                    continue;
                }
                let half = inside.sqrt();
                let x0 = (cx - half) as i32;
                let x1 = (cx + half) as i32;
                for x in x0..=x1 {
                    surface.put(x, y, c);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render `n` frames from 0 on a fresh instance and collect checksums.
    fn checksums(mut e: impl Effect, n: u64) -> Vec<u64> {
        let mut s = Surface::new(64, 48);
        (0..n)
            .map(|f| {
                e.render(&mut s, f);
                s.checksum()
            })
            .collect()
    }

    #[test]
    fn a_fresh_surface_is_transparent_black_and_the_right_size() {
        let s = Surface::new(64, 48);
        assert_eq!(s.stride, 64 * 4);
        assert_eq!(s.byte_len(), 64 * 48 * 4);
        assert!(s.data.iter().all(|&b| b == 0));
    }

    #[test]
    fn putting_a_pixel_off_the_edge_neither_panics_nor_writes() {
        let mut s = Surface::new(8, 4);
        let before = s.checksum();
        for (x, y) in [(-1, 0), (0, -1), (8, 0), (0, 4), (-99, -99), (999, 999)] {
            s.put(x, y, 0xffff_ffff);
        }
        assert_eq!(s.checksum(), before);
    }

    #[test]
    fn the_checksum_is_stable_for_equal_content_and_differs_otherwise() {
        let mut a = Surface::new(8, 8);
        let mut b = Surface::new(8, 8);
        a.fill(bgra(1, 2, 3));
        b.fill(bgra(1, 2, 3));
        assert_eq!(a.checksum(), b.checksum());
        b.put(3, 3, bgra(9, 9, 9));
        assert_ne!(a.checksum(), b.checksum());
    }

    #[test]
    fn every_effect_repeats_itself_from_a_fresh_instance() {
        assert_eq!(checksums(Plasma::new(), 4), checksums(Plasma::new(), 4));
        assert_eq!(
            checksums(Fire::new(64, 48), 6),
            checksums(Fire::new(64, 48), 6)
        );
        assert_eq!(checksums(Rotozoom::new(), 4), checksums(Rotozoom::new(), 4));
        assert_eq!(
            checksums(Boing::new(64, 48), 4),
            checksums(Boing::new(64, 48), 4)
        );
        assert_eq!(
            checksums(Starfield::new(40, 64, 48), 6),
            checksums(Starfield::new(40, 64, 48), 6)
        );
        assert_eq!(
            checksums(Balls::new(6, 64, 48), 6),
            checksums(Balls::new(6, 64, 48), 6)
        );
    }

    #[test]
    fn every_effect_has_a_name_and_changes_something() {
        let mut s = Surface::new(64, 48);
        let blank = s.checksum();
        let mut effects: Vec<Box<dyn Effect>> = vec![
            Box::new(Plasma::new()),
            Box::new(Fire::new(64, 48)),
            Box::new(Rotozoom::new()),
            Box::new(Boing::new(64, 48)),
            Box::new(Starfield::new(40, 64, 48)),
            Box::new(Balls::new(6, 64, 48)),
        ];
        for e in &mut effects {
            assert!(!e.name().is_empty());
            e.render(&mut s, 1);
            assert_ne!(s.checksum(), blank, "{} drew nothing", e.name());
        }
    }

    #[test]
    fn the_plasma_and_the_rotozoom_do_not_remember_earlier_frames() {
        let mut direct = Surface::new(64, 48);
        let mut walked = Surface::new(64, 48);

        let mut p = Plasma::new();
        p.render(&mut direct, 3);
        let mut q = Plasma::new();
        for f in 0..=3 {
            q.render(&mut walked, f);
        }
        assert_eq!(direct.checksum(), walked.checksum());

        let mut r = Rotozoom::new();
        r.render(&mut direct, 3);
        let mut t = Rotozoom::new();
        for f in 0..=3 {
            t.render(&mut walked, f);
        }
        assert_eq!(direct.checksum(), walked.checksum());
    }

    #[test]
    fn the_fire_carries_state_that_a_different_seed_changes() {
        let mut a = Surface::new(64, 48);
        let mut b = Surface::new(64, 48);
        let mut one = Fire::seeded(64, 48, 1);
        let mut two = Fire::seeded(64, 48, 2);
        for f in 0..=5u64 {
            one.render(&mut a, f);
            two.render(&mut b, f);
        }
        // Same size, same frame, same code path: only the PRNG stream
        // differs, so any difference at all is the automaton's state.
        assert_ne!(a.checksum(), b.checksum());
    }

    #[test]
    fn the_fire_cannot_be_asked_for_an_earlier_frame() {
        let mut s = Surface::new(64, 48);
        let mut f = Fire::new(64, 48);
        for n in 0..=6u64 {
            f.render(&mut s, n);
        }
        let at_six = s.checksum();
        // Going backwards has no meaning for a cooling automaton, so the
        // documented answer is "you get where you are", not a rewind.
        f.render(&mut s, 2);
        assert_eq!(s.checksum(), at_six);

        // And a fresh fire at frame 2 is a genuinely different picture,
        // which is the whole point of the exception.
        let mut fresh = Surface::new(64, 48);
        Fire::new(64, 48).render(&mut fresh, 2);
        assert_ne!(fresh.checksum(), at_six);
    }

    #[test]
    fn re_rendering_the_same_fire_frame_is_a_redraw_not_a_step() {
        let mut s = Surface::new(64, 48);
        let mut f = Fire::new(64, 48);
        for n in 0..=3 {
            f.render(&mut s, n);
        }
        let once = s.checksum();
        f.render(&mut s, 3);
        assert_eq!(s.checksum(), once);
    }

    #[test]
    fn the_fire_cools_to_black_at_the_top() {
        let mut s = Surface::new(64, 48);
        let mut f = Fire::new(64, 48);
        for n in 0..4 {
            f.render(&mut s, n);
        }
        let black = bgra(0, 0, 0);
        for x in 0..64i32 {
            let off = (x as usize) * 4;
            let px = u32::from_le_bytes(s.data[off..off + 4].try_into().unwrap());
            assert_eq!(px, black, "top row pixel {x} is lit after 4 frames");
        }
    }

    #[test]
    fn the_fire_is_hot_along_the_bottom_row() {
        let mut s = Surface::new(64, 48);
        let mut f = Fire::new(64, 48);
        f.render(&mut s, 0);
        let row = (47 * s.stride) as usize;
        for x in 0..64usize {
            let off = row + x * 4;
            let px = u32::from_le_bytes(s.data[off..off + 4].try_into().unwrap());
            assert_eq!(px, bgra(255, 255, 255), "bottom row pixel {x} is cold");
        }
    }

    #[test]
    fn the_fire_palette_runs_from_black_to_white() {
        let p = fire_palette();
        assert_eq!(p[0], bgra(0, 0, 0));
        assert_eq!(p[255], bgra(255, 255, 255));
        // Red climbs first: at a quarter of the range there is red and
        // nothing else worth calling green.
        let mid = p[63];
        assert!((mid >> 16) & 0xff > 200, "no red at the bottom of the ramp");
        assert_eq!(mid & 0xff, 0, "blue appeared too early");
    }

    #[test]
    fn the_plasma_palette_is_opaque_everywhere() {
        let p = plasma_palette();
        assert_eq!(p.len(), 256);
        assert!(p.iter().all(|c| c >> 24 == 0xff));
        assert!(p[0] != p[128], "the palette is flat");
    }

    #[test]
    fn the_colour_tables_are_opaque_and_distinct() {
        for table in [BALL_COLORS, BENCH_COLORS] {
            assert!(table.iter().all(|c| c >> 24 == 0xff));
            for i in 0..table.len() {
                for j in i + 1..table.len() {
                    assert_ne!(table[i], table[j], "duplicate colour at {i}/{j}");
                }
            }
        }
    }

    #[test]
    fn the_boing_sprite_is_transparent_in_the_corners_and_opaque_at_the_centre() {
        let b = Boing::new(64, 48);
        let s = b.sprite(32, 0);
        let alpha_at = |x: usize, y: usize| {
            let off = y * (s.stride as usize) + x * 4;
            s.data[off + 3]
        };
        for (x, y) in [(0, 0), (31, 0), (0, 31), (31, 31)] {
            assert_eq!(alpha_at(x, y), 0, "corner ({x},{y}) is not transparent");
        }
        assert_eq!(
            alpha_at(16, 16),
            255,
            "the centre of the ball is not opaque"
        );
    }

    #[test]
    fn the_boing_sprite_has_both_a_red_and_a_white_chequer() {
        let b = Boing::new(64, 48);
        let s = b.sprite(48, 7);
        let mut reddish = 0;
        let mut whitish = 0;
        for px in s.data.chunks_exact(4) {
            if px[3] == 0 {
                continue;
            }
            let (bl, g, r) = (px[0], px[1], px[2]);
            if r > 120 && g < 100 && bl < 110 {
                reddish += 1;
            } else if r > 120 && g > 120 && bl > 120 {
                whitish += 1;
            }
        }
        assert!(reddish > 20, "no red chequers: {reddish}");
        assert!(whitish > 20, "no white chequers: {whitish}");
    }

    #[test]
    fn the_boing_ball_stays_in_the_box_and_actually_moves() {
        let b = Boing::new(320, 200);
        let (x0, y0, _) = b.position(0);
        let mut moved = false;
        for f in 0..200u64 {
            let (x, y, r) = b.position(f);
            assert!(x - r >= -0.5 && x + r <= 320.5, "frame {f}: x {x} r {r}");
            assert!(y - r >= -0.5 && y + r <= 200.5, "frame {f}: y {y} r {r}");
            if (x - x0).abs() > 1.0 || (y - y0).abs() > 1.0 {
                moved = true;
            }
        }
        assert!(moved, "the ball never left its starting position");
    }

    #[test]
    fn the_boing_position_depends_only_on_the_frame() {
        let b = Boing::new(320, 200);
        assert_eq!(b.position(37), b.position(37));
        assert_ne!(b.position(37), b.position(38));
    }

    #[test]
    fn every_star_is_inside_the_surface_and_the_count_is_respected() {
        let mut f = Starfield::new(64, 96, 72);
        for n in 0..6u64 {
            let stars = f.stars(n);
            assert_eq!(stars.len(), 64);
            for s in stars {
                assert!((0.0..96.0).contains(&s.x), "x out of range: {}", s.x);
                assert!((0.0..72.0).contains(&s.y), "y out of range: {}", s.y);
                assert!(s.size >= 1.0);
            }
        }
    }

    #[test]
    fn stars_move_as_frames_pass() {
        let mut f = Starfield::new(32, 96, 72);
        let first: Vec<(f32, f32)> = f.stars(0).iter().map(|s| (s.x, s.y)).collect();
        let later: Vec<(f32, f32)> = f.stars(6).iter().map(|s| (s.x, s.y)).collect();
        assert!(
            first.iter().zip(&later).any(|(a, b)| a != b),
            "no star moved in six frames"
        );
    }

    #[test]
    fn a_starfield_with_no_stars_still_renders() {
        let mut s = Surface::new(16, 16);
        let mut f = Starfield::new(0, 16, 16);
        f.render(&mut s, 0);
        assert!(f.stars(1).is_empty());
    }

    #[test]
    fn every_ball_stays_inside_the_box_over_a_long_run() {
        let mut b = Balls::new(12, 200, 150);
        for n in 0..100u64 {
            for ball in b.balls(n) {
                assert!(ball.x >= -0.5, "frame {n}: left edge {}", ball.x);
                assert!(ball.y >= -0.5, "frame {n}: top edge {}", ball.y);
                assert!(
                    ball.x + ball.d <= 200.5,
                    "frame {n}: right edge {}",
                    ball.x + ball.d
                );
                assert!(
                    ball.y + ball.d <= 150.5,
                    "frame {n}: bottom edge {}",
                    ball.y + ball.d
                );
                assert!(ball.d > 0.0);
                assert!(ball.color < BALL_COLORS.len());
            }
        }
    }

    #[test]
    fn balls_keep_bouncing_rather_than_settling() {
        let mut b = Balls::new(8, 200, 150);
        let start: Vec<f32> = b.balls(0).iter().map(|x| x.y).collect();
        let end: Vec<f32> = b.balls(60).iter().map(|x| x.y).collect();
        assert!(
            start.iter().zip(&end).any(|(a, c)| (a - c).abs() > 1.0),
            "every ball is where it started after sixty frames"
        );
    }

    #[test]
    fn the_rotozoom_texture_wraps_instead_of_clamping() {
        // A surface much larger than the 64×64 texture must show the
        // texture repeated, not a stretched border: sampling far off the
        // texture's origin has to land back inside it.
        let r = Rotozoom::new();
        let a = r.sample(0, 0);
        let b = r.sample(ROTO_EDGE << 16, ROTO_EDGE << 16);
        let c = r.sample(-(ROTO_EDGE << 16), -(ROTO_EDGE << 16));
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn the_sine_table_looks_like_a_sine() {
        let table = sin_table();
        let len = SIN_LEN as usize;
        assert_eq!(table.len(), len);
        assert!(table[0].abs() < 1e-5);
        assert!((table[len / 4] - 1.0).abs() < 1e-3);
        assert!((table[3 * len / 4] + 1.0).abs() < 1e-3);
        // The wrapping lookup must agree with the raw table for negative
        // phases, which is the case every centred effect hits. Compared
        // exactly rather than approximately on purpose: `tsin` must be a
        // *lookup*, so it has to return the table's own bits.
        assert_eq!(tsin(&table, -1).to_bits(), table[len - 1].to_bits());
        assert_eq!(tsin(&table, SIN_LEN + 3).to_bits(), table[3].to_bits());
    }

    #[test]
    fn the_prng_is_seeded_deterministically_and_does_not_stick_at_zero() {
        let mut a = Rng::new(0);
        let mut b = Rng::new(0);
        let xs: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);
        assert!(xs.iter().any(|&v| v != xs[0]), "the stream is constant");
        let mut c = Rng::new(1);
        assert_ne!(c.next_u64(), xs[0]);
        assert!((0..32).all(|_| a.next_f32() < 1.0));
        assert!((0..32).all(|_| a.below(3) < 3));
    }

    #[test]
    fn a_zero_sized_surface_is_harmless() {
        let mut s = Surface::new(0, 0);
        s.fill(bgra(1, 2, 3));
        s.put(0, 0, bgra(4, 5, 6));
        assert_eq!(s.byte_len(), 0);
        let b = Boing::new(0, 0);
        assert_eq!(b.sprite(0, 0).byte_len(), 0);
    }
}
