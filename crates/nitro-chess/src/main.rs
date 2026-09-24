//! The binary: connect, open the window, run the loop. Everything else
//! is in the library, so the tests drive the tree this builds.

fn main() -> Result<(), nitro_ui::Error> {
    nitro_chess::run()
}
