//! Build `src/shim.c` against the system `FFmpeg` (#3906).
//!
//! Deliberately without the `cc` and `pkg-config` crates: the build asks
//! the system's own `pkg-config` for the flags, compiles one C file with
//! the system `cc`, and archives it with `ar` — three commands, zero
//! crates. libavformat, libavcodec and libavutil are linked
//! **dynamically**; nothing of `FFmpeg` is vendored or static.

use std::path::PathBuf;
use std::process::Command;

const LIBS: [&str; 3] = ["libavformat", "libavcodec", "libavutil"];

fn run(cmd: &mut Command) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("nitro-video build: running {cmd:?}: {e}"));
    assert!(
        out.status.success(),
        "nitro-video build: {cmd:?} failed:\n{}\n\
         nitro-video links the system FFmpeg: install libavformat-dev, \
         libavcodec-dev and libavutil-dev (see DEPENDENCIES.md)",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn main() {
    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let cflags = run(Command::new("pkg-config").arg("--cflags").args(LIBS));
    let libs = run(Command::new("pkg-config").arg("--libs").args(LIBS));
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let obj = out.join("shim.o");
    run(Command::new(&cc)
        .args(["-c", "-O2", "-fPIC", "-Wall", "-Wextra", "-std=c11"])
        .args(cflags.split_whitespace())
        .arg("src/shim.c")
        .arg("-o")
        .arg(&obj));
    let lib = out.join("libnitro_video_shim.a");
    let _ = std::fs::remove_file(&lib);
    run(Command::new("ar").arg("crs").arg(&lib).arg(&obj));
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=nitro_video_shim");
    for flag in libs.split_whitespace() {
        if let Some(dir) = flag.strip_prefix("-L") {
            println!("cargo:rustc-link-search=native={dir}");
        } else if let Some(name) = flag.strip_prefix("-l") {
            println!("cargo:rustc-link-lib=dylib={name}");
        }
    }
}
