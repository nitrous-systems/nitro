//! `nitro-auth`: the lock screen's PAM helper.
//!
//! It authenticates **the session's own user** and nobody else, and it
//! speaks greetd's IPC ([`nitro_login::ipc`]) on stdin and stdout, so
//! `nitro-greeter` runs one conversation state machine against either
//! this or greetd. It is the only binary in the tree that links libpam.
//!
//! [`serve::serve`] is the loop, generic over an
//! [`Authenticator`](serve::Authenticator) so it is tested without PAM;
//! [`pam::Pam`] is the real one.

pub mod pam;
pub mod serve;
