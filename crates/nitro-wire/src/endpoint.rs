//! Where a client connects: a Unix path, or a TCP address.
//!
//! `NITRO_SOCKET` has meant "a filesystem path" since M1. From M4-E1 it
//! also accepts `tcp://host:port`, and [`Endpoint`] is the parsed answer.
//! The protocol itself is unchanged — the framing never depended on the
//! socket being local (`docs/wire.md` §Transport) — so the only thing that
//! differs downstream is that a remote link cannot carry file descriptors.
//!
//! # Why a prefix and not a second variable
//!
//! One variable names the server, and a client that is told where to
//! connect should not also have to be told *how*. `tcp://` is the one
//! spelling nobody mistakes for a path (a path may not contain it: a
//! relative path starting `tcp://` would have an empty first component,
//! which no filesystem produces), and everything else stays a path
//! verbatim — including an absolute path with colons in it, which is why
//! the check is a prefix and not a "does it look like host:port" guess.
//!
//! # Resolution
//!
//! A host may be an IPv4 or IPv6 literal, a bracketed IPv6 literal, or a
//! name. Names go through [`std::net::ToSocketAddrs`], which is the
//! platform resolver, and every address it returns is tried in order —
//! the first that *connects* wins, which is what a dual-stack box with a
//! v6 address and no v6 route needs.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// The `tcp://` scheme prefix `NITRO_SOCKET` understands.
pub const TCP_SCHEME: &str = "tcp://";

/// Where the server is: a Unix socket path, or a TCP address.
///
/// [`Endpoint::parse`] never fails and never panics — a value it cannot
/// make sense of as TCP is not an error here but a *path*, and a path is
/// only an error when the connection is attempted. That keeps the failure
/// where the user can act on it ("No such file or directory: …") instead
/// of turning a typo into an opaque parse error at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix `SOCK_STREAM` socket at this path. The M1 behaviour, and
    /// still the default.
    Unix(PathBuf),
    /// A TCP address, from `tcp://host:port`. The host has already been
    /// resolved: a name that resolves to several addresses becomes
    /// several candidates, tried in order.
    Tcp(Vec<SocketAddr>),
}

impl Endpoint {
    /// Parse one `NITRO_SOCKET`-style value.
    ///
    /// `tcp://host:port` resolves the host and yields [`Endpoint::Tcp`];
    /// anything else is [`Endpoint::Unix`] of the value verbatim.
    ///
    /// # Errors
    /// [`Error::BadEndpoint`] only for a value that *claims* to be TCP —
    /// it starts with `tcp://` — and is not usable: no port, a port out
    /// of range, an unbracketed IPv6 literal, or a host the resolver does
    /// not know. Saying "this is not a hostname" is more useful than
    /// silently trying to open a file called `tcp://nosuchbox:7700`.
    pub fn parse(value: &str) -> Result<Self, Error> {
        let Some(rest) = value.strip_prefix(TCP_SCHEME) else {
            return Ok(Self::Unix(PathBuf::from(value)));
        };
        let addrs = resolve(rest)?;
        Ok(Self::Tcp(addrs))
    }

    /// Whether this endpoint is remote, i.e. cannot carry descriptors.
    #[must_use]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Tcp(_))
    }
}

/// Resolve `host:port` to at least one socket address.
///
/// Split at the **last** colon so an unbracketed IPv6 literal is caught
/// rather than mangled: `::1:7700` would otherwise resolve as host `::1`
/// only by accident of where the split landed, and a user who wrote it
/// deserves to be told the bracketed spelling.
fn resolve(hostport: &str) -> Result<Vec<SocketAddr>, Error> {
    use std::net::ToSocketAddrs as _;

    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| Error::BadEndpoint(format!("{hostport:?} is not `host:port`")))?;
    if host.is_empty() {
        return Err(Error::BadEndpoint(format!("{hostport:?} names no host")));
    }
    // A bare `::1:7700` splits into host `::1` and port `7700`, which
    // *looks* right and is not: `[::1]:7700` is the spelling every other
    // tool uses, and accepting both would make `::1:2:3` ambiguous.
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err(Error::BadEndpoint(format!(
            "{hostport:?}: bracket an IPv6 literal, e.g. `[::1]:7700`"
        )));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| Error::BadEndpoint(format!("{port:?} is not a port number")))?;
    if port == 0 {
        return Err(Error::BadEndpoint("port 0 is not connectable".to_owned()));
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = (bare, port)
        .to_socket_addrs()
        .map_err(|e| Error::BadEndpoint(format!("cannot resolve {host:?}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::BadEndpoint(format!(
            "{host:?} resolved to no addresses"
        )));
    }
    Ok(addrs)
}

/// Parse a **bind** address — what `server.conf`'s `remote.listen` holds.
///
/// Deliberately not [`Endpoint::parse`]: a listener is not a connection.
/// There is no `tcp://` prefix (the key is already about TCP), port 0 is
/// legal and means "ask the kernel" (which is how the tests get a port),
/// and exactly one address is wanted — a bind that resolved to three
/// would silently pick one.
///
/// # Errors
/// [`Error::BadEndpoint`] for anything not `addr:port`.
pub fn parse_listen(value: &str) -> Result<SocketAddr, Error> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| Error::BadEndpoint(format!("{value:?} is not `addr:port`")))?;
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err(Error::BadEndpoint(format!(
            "{value:?}: bracket an IPv6 literal, e.g. `[::1]:7700`"
        )));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| Error::BadEndpoint(format!("{port:?} is not a port number")))?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    // A *literal* only. A listener resolved through DNS is a foot-gun: the
    // name may move, and the address the server bound is then not the one
    // the config names.
    let ip: std::net::IpAddr = bare
        .parse()
        .map_err(|_| Error::BadEndpoint(format!("{bare:?} is not an IP address literal")))?;
    Ok(SocketAddr::new(ip, port))
}

/// Where the wire endpoint is: `NITRO_SOCKET` if set, else the default
/// Unix path from [`socket_path`](crate::socket_path).
///
/// # Errors
/// As [`Endpoint::parse`].
pub fn endpoint() -> Result<Endpoint, Error> {
    match std::env::var_os(crate::SOCKET_ENV) {
        Some(v) => Endpoint::parse(&v.to_string_lossy()),
        None => Ok(Endpoint::Unix(crate::socket_path())),
    }
}

/// Where the **shell** endpoint is.
///
/// Always a path, never TCP: `caps::SHELL` is granted because the client
/// could open a `0700` path, and a TCP port carries no such proof. A
/// `tcp://` in `NITRO_SHELL_SOCKET` is therefore an error rather than a
/// silent downgrade — see `docs/shell.md`.
///
/// # Errors
/// [`Error::BadEndpoint`] if `NITRO_SHELL_SOCKET` names a TCP endpoint.
pub fn shell_endpoint() -> Result<PathBuf, Error> {
    let path = crate::shell_socket_path();
    if path.to_string_lossy().starts_with(TCP_SCHEME) {
        return Err(Error::BadEndpoint(
            "the shell socket cannot be remote: the 0700 path is the privilege (docs/shell.md)"
                .to_owned(),
        ));
    }
    Ok(path)
}

/// Format an endpoint for a log line.
impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(p) => write!(f, "{}", Path::new(p).display()),
            Self::Tcp(addrs) => {
                write!(f, "{TCP_SCHEME}")?;
                for (i, a) in addrs.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{a}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_value_is_a_path() {
        assert_eq!(
            Endpoint::parse("/run/user/1000/nitro/wire.sock").unwrap(),
            Endpoint::Unix(PathBuf::from("/run/user/1000/nitro/wire.sock"))
        );
        // Colons in a path are a path, not an address.
        assert_eq!(
            Endpoint::parse("/tmp/weird:name.sock").unwrap(),
            Endpoint::Unix(PathBuf::from("/tmp/weird:name.sock"))
        );
        assert!(!Endpoint::parse("/tmp/x").unwrap().is_remote());
    }

    #[test]
    fn tcp_literals_resolve_without_a_resolver() {
        let v4 = Endpoint::parse("tcp://127.0.0.1:7700").unwrap();
        assert_eq!(
            v4,
            Endpoint::Tcp(vec!["127.0.0.1:7700".parse::<SocketAddr>().unwrap()])
        );
        assert!(v4.is_remote());
        let v6 = Endpoint::parse("tcp://[::1]:7700").unwrap();
        assert_eq!(
            v6,
            Endpoint::Tcp(vec!["[::1]:7700".parse::<SocketAddr>().unwrap()])
        );
        assert_eq!(v4.to_string(), "tcp://127.0.0.1:7700");
    }

    #[test]
    fn a_name_goes_through_the_platform_resolver() {
        // `localhost` is the one name every box is supposed to have, and
        // it is *still* not asserted to resolve: a sandbox with no
        // `/etc/hosts` and no resolver is a legitimate place to run the
        // suite, and a test that fails there would be testing the build
        // machine's networking rather than this parser. What is pinned is
        // the shape of both answers — every address carries the port, and
        // a failure is a `BadEndpoint` naming the host rather than a
        // panic or a silent fallback to a path.
        match Endpoint::parse("tcp://localhost:7700") {
            Ok(Endpoint::Tcp(addrs)) => {
                assert!(!addrs.is_empty());
                assert!(addrs.iter().all(|a| a.port() == 7700));
            }
            Ok(other) => panic!("a tcp:// value is never a path: {other:?}"),
            Err(Error::BadEndpoint(msg)) => assert!(msg.contains("localhost"), "{msg}"),
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn garbage_never_panics() {
        for text in [
            "tcp://",
            "tcp://:",
            "tcp://:7700",
            "tcp://host",
            "tcp://host:",
            "tcp://host:port",
            "tcp://127.0.0.1:0",
            "tcp://127.0.0.1:99999",
            "tcp://127.0.0.1:-1",
            "tcp://::1:7700",
            "tcp://[::1]",
            "tcp://[::1]:",
            "tcp://nosuchhost.invalid:7700",
            "tcp://\u{1f4a9}:7700",
            "tcp://127.0.0.1:7700:7700",
        ] {
            // Every one of these is an error, and none of them panics.
            assert!(Endpoint::parse(text).is_err(), "{text:?} must not parse");
        }
        // And the non-`tcp://` half is never an error at all.
        for text in ["", "tcp:/x", "TCP://127.0.0.1:7700", "::1:7700", "\0"] {
            assert!(matches!(Endpoint::parse(text).unwrap(), Endpoint::Unix(_)));
        }
    }

    #[test]
    fn a_listen_address_is_a_literal_and_may_be_port_zero() {
        assert_eq!(
            parse_listen("127.0.0.1:0").unwrap(),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_listen("0.0.0.0:7700").unwrap(),
            "0.0.0.0:7700".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            parse_listen("[::1]:7700").unwrap(),
            "[::1]:7700".parse::<SocketAddr>().unwrap()
        );
        for bad in [
            "",
            "7700",
            "localhost:7700",
            "127.0.0.1",
            "127.0.0.1:",
            "127.0.0.1:x",
            "::1:7700",
            "tcp://127.0.0.1:7700",
        ] {
            assert!(parse_listen(bad).is_err(), "{bad:?} must not parse");
        }
    }
}
