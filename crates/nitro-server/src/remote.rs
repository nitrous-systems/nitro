//! The **remote** listener: the same wire, over TCP.
//!
//! An app on machine B connects to the server on machine A and sends the
//! same mutation stream it sends over the Unix socket. No pixels cross
//! the link — an `Image`'s buffer rides on `SCM_RIGHTS`, which TCP does
//! not have — so buffer ops are refused and everything else is identical:
//! the same `clients.rs` node ownership, the same damage, the same window
//! management, the same decorations. `docs/remote.md` has the model, the
//! security model and the numbers.
//!
//! # Opt-in, and off by default
//!
//! No `remote.listen` in `server.conf` means no socket, no epoll
//! registration and no accept path — a desktop that does not want remote
//! clients pays nothing for the feature existing. The key is applied at
//! startup *and* on reload, so turning it on does not need a restart.
//!
//! # There is no authentication
//!
//! None. A process that can open the port is a client, with `WM` and
//! `TEXT` and the ability to put windows on the user's screen. The
//! documented model is therefore **loopback plus an SSH port-forward**:
//!
//! ```console
//! $ ssh -L 7700:127.0.0.1:7700 box -N      # on the client machine
//! $ NITRO_SOCKET=tcp://127.0.0.1:7700 nitro-calc
//! ```
//!
//! which puts authentication where there already is some — in sshd — and
//! leaves nitro with one job. A non-loopback bind is for measurement on a
//! trusted LAN and warns loudly on every bind, saying exactly this.

use std::net::SocketAddr;

use nitro_wire::server::TcpListener;

use crate::warn;

/// The server's remote listener, if `remote.listen` asked for one.
///
/// A thin wrapper over [`TcpListener`] that remembers the address the
/// configuration *asked* for alongside the one that was actually bound.
/// The two differ whenever the port is 0, and the difference is what
/// makes a reload able to answer "is this the same listener?" without
/// rebinding to find out.
#[derive(Debug)]
pub struct RemoteListener {
    listener: TcpListener,
    /// What `remote.listen` said, before the kernel resolved a port.
    configured: SocketAddr,
}

impl RemoteListener {
    /// Bind `addr`, warning once if it is not a loopback address.
    ///
    /// # Errors
    /// Any bind failure — `EADDRINUSE` for a port in use, `EACCES` for a
    /// privileged one. The caller warns and runs *without* a remote
    /// listener: a typo in `remote.listen` must not cost the user their
    /// desktop.
    pub fn bind(addr: SocketAddr) -> Result<Self, nitro_wire::Error> {
        let listener = TcpListener::bind(addr)?;
        if !addr.ip().is_loopback() {
            warn!(
                "remote.listen {addr} is not loopback: nitro has NO authentication, \
                 anyone who can reach this port can put windows on your screen. \
                 The supported model is loopback + `ssh -L {}:127.0.0.1:{} host` \
                 (docs/remote.md).",
                addr.port(),
                addr.port()
            );
        }
        Ok(Self {
            listener,
            configured: addr,
        })
    }

    /// The listener, for accept and epoll registration.
    #[must_use]
    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }

    /// The address actually bound, with the port the kernel chose.
    #[must_use]
    pub fn addr(&self) -> SocketAddr {
        self.listener.addr()
    }

    /// Whether a new configuration asks for the same listener this one
    /// already is.
    ///
    /// Compared against what the *file* said, not against the bound
    /// address, and that is the point: `127.0.0.1:0` stays satisfied by
    /// the listener it produced, so a reload that changed only the
    /// keyboard does not rebind the port — and every connected remote
    /// client keeps its connection, because the socket it was accepted on
    /// is untouched.
    #[must_use]
    pub fn satisfies(&self, addr: SocketAddr) -> bool {
        self.configured == addr
    }
}

/// How `stats` reports the listener: the bound address, or `off`.
///
/// The **bound** one, so a test that configured `:0` can read the port
/// the kernel chose and connect to it. That is not a test affordance
/// bolted on: a person who wrote `remote.listen = 127.0.0.1:0` has
/// exactly the same question, and "the address in the file" would be a
/// useless answer to it.
#[must_use]
pub fn listen_text(listener: Option<&RemoteListener>) -> String {
    match listener {
        Some(l) => l.addr().to_string(),
        None => "off".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_of_zero_is_resolved_and_still_satisfied_by_its_own_config() {
        let asked: SocketAddr = "127.0.0.1:0".parse().expect("literal");
        let l = RemoteListener::bind(asked).expect("bind loopback");
        assert_ne!(l.addr().port(), 0, "the kernel resolved a port");
        assert_eq!(l.addr().ip(), asked.ip());
        // A reload that re-reads the same file must not rebind.
        assert!(l.satisfies(asked));
        // A different address must.
        assert!(!l.satisfies("127.0.0.1:7700".parse().expect("literal")));
        assert_eq!(listen_text(Some(&l)), l.addr().to_string());
        assert_eq!(listen_text(None), "off");
    }

    #[test]
    fn a_port_already_in_use_is_an_error_and_not_a_panic() {
        let first = RemoteListener::bind("127.0.0.1:0".parse().expect("literal")).expect("bind");
        let taken = first.addr();
        // `SO_REUSEADDR` does not let two listeners share a port; only
        // `SO_REUSEPORT` would, and it is deliberately not set.
        assert!(RemoteListener::bind(taken).is_err());
    }
}
