//! The binary: connect, open the window, run the loop.
//!
//! Everything else lives in the library next door, so the tests drive the
//! same tree this builds rather than a copy of it. See the crate
//! documentation in `lib.rs` for what the calculator is and how `hey`
//! drives it.

fn main() -> Result<(), nitro_ui::Error> {
    nitro_calc::run()
}
