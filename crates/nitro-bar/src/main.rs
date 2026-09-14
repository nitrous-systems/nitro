//! The binary: connect to the shell socket, open the bar, run the loop.
//!
//! Everything else lives in the library next door, so the tests drive the
//! same tree this builds rather than a copy of it. See the crate
//! documentation in `lib.rs`.

fn main() -> Result<(), nitro_ui::Error> {
    nitro_bar::run()
}
