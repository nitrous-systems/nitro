//! The login wire: greetd's IPC, shared by both ends of nitro's lock
//! screen and greeter.
//!
//! `nitro-auth` (the PAM helper) speaks this protocol on its stdin and
//! stdout; greetd speaks it on `$GREETD_SOCK`. `nitro-greeter` talks to
//! either through the same codec, and so without linking libpam: the
//! helper is the only binary that does. See `docs/greeter.md`.

pub mod ipc;
pub mod owner;

pub use ipc::{
    ErrorKind, Framer, MAX_FRAME, MessageKind, ProtocolError, Request, Response, decode_request,
    decode_response, encode_request, encode_response,
};
pub use owner::owner;
