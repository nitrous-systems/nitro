//! The binary: connect to the shell socket, open the backdrop, run the
//! loop.
//!
//! Everything else lives in the library next door, so the tests drive the
//! same tree this builds rather than a copy of it. See the crate
//! documentation in `lib.rs`.

fn main() -> Result<(), nitro_ui::Error> {
    // `skip(1)` drops argv[0]; everything else is the wallpaper's.
    let args: Vec<String> = std::env::args().skip(1).collect();
    nitro_wallpaper::run(&args)
}
