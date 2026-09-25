//! The binary: read the command line, connect, run.
//!
//! Everything else is in the library, so the tests drive the same tree
//! this builds. See the crate documentation in `lib.rs`.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let paths: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    nitro_amp::run(&paths)
}
