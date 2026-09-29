//! `nitro-gpu-vulkan`: the GPU helper process.
//!
//! The server starts it with the helper socket as **fd 0** and nothing
//! else of interest inherited. It drops privileges, restricts the Vulkan
//! loader to the render node's vendor ICD, opens the device, checks that
//! it holds no DRM primary node or input device, and runs the `nitro-gpu`
//! event loop until the server closes the socket.
//!
//! Environment:
//! - `NITRO_GPU_RENDER_NODE`: render node (default: first `/dev/dri/renderD*`);
//! - `NITRO_GPU_ICD`: one ICD manifest, overriding the vendor lookup;
//! - `NITRO_GPU_SHADOW=staging`: never try udmabuf for the shadow;
//! - `NITRO_GPU_IDLE_EXIT=<secs>`: on-demand mode (exit when idle).

#![deny(clippy::undocumented_unsafe_blocks)]

use std::process::ExitCode;
use std::time::Duration;

use nitro_gpu::{Config, sandbox};
use nitro_gpu_vulkan::{VkBackend, icd};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nitro-gpu-vulkan: {e}");
            ExitCode::FAILURE
        }
    }
}

// The one `unsafe` of this file: `env::set_var` before any thread exists.
#[allow(unsafe_code)]
fn run() -> Result<(), String> {
    let forbidden = sandbox::check_fds();
    if !forbidden.is_empty() {
        return Err(format!(
            "refusing to run with inherited device fds: {forbidden:?}"
        ));
    }
    let sock = sandbox::take_stdin().map_err(|e| format!("socket on fd 0: {e}"))?;
    let sock = nitro_wire::Socket::from_fd(sock).map_err(|e| format!("socket on fd 0: {e}"))?;
    sandbox::apply().map_err(|e| format!("sandbox: {e}"))?;

    let node = icd::render_node().ok_or("no render node")?;
    let candidates = icd::for_node(&node);
    if candidates.is_empty() {
        return Err(format!("no vendor Vulkan driver for {}", node.display()));
    }
    let mut last = String::new();
    let mut backend = None;
    for icd in &candidates {
        // SAFETY: `set_var` is unsound only with concurrent environment
        // access from another thread. This runs on the main thread before
        // any thread exists: the helper spawns none, and the Vulkan loader
        // is loaded (and may start driver threads) only after this call,
        // inside `VkBackend::open`. A failed attempt tears its instance
        // down before the next `set_var`.
        unsafe { std::env::set_var("VK_DRIVER_FILES", icd) };
        match VkBackend::open(&node) {
            Ok(b) => {
                backend = Some(b);
                break;
            }
            Err(e) => last = format!("{}: {e}", icd.display()),
        }
    }
    let backend = backend.ok_or(last)?;
    let bad = sandbox::check_fds();
    if !bad.is_empty() {
        return Err(format!("driver opened forbidden fds: {bad:?}"));
    }
    let idle_exit = std::env::var("NITRO_GPU_IDLE_EXIT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs);
    nitro_gpu::run(sock, backend, Config { idle_exit }).map_err(|e| e.to_string())?;
    Ok(())
}
