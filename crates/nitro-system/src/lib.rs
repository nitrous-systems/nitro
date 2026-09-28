//! The desktop's remote controls, shared by the apps that press them.
//!
//! nitro owns neither the mixer nor the power button. It *drives* them:
//! [`audio`] runs `wpctl`/`pactl`, [`conf`] reads and writes the
//! compositor's `server.conf`, and [`session`] asks `nitro-session` to
//! suspend, reboot, power off or log out. `nitro-settings` and
//! `nitro-bar`'s quick-settings menu both need all three, so they live
//! here rather than in either app.
//!
//! The crate depends on `nitro-core` alone (for [`nitro_core::Scheme`]),
//! adds no external crate, and has no background activity: every
//! function runs when it is called and returns, so a consumer's idle
//! contract is its own to keep.

pub mod audio;
pub mod conf;
pub mod session;
