//! greetd's IPC, hand-rolled: the messages, a JSON codec for exactly
//! them, and the length-prefixed framing.
//!
//! A frame is a **native-endian `u32` length** followed by that many
//! bytes of one flat JSON object tagged by `"type"`:
//!
//! | direction | `type` | fields |
//! |---|---|---|
//! | → | `create_session` | `username` |
//! | → | `post_auth_message_response` | `response`: string or `null` |
//! | → | `start_session` | `cmd`: `[string]`, `env`: `[string]` |
//! | → | `cancel_session` | — |
//! | ← | `success` | — |
//! | ← | `error` | `error_type`: `auth_error` \| `error`, `description` |
//! | ← | `auth_message` | `auth_message_type`: `visible` \| `secret` \| `info` \| `error`, `auth_message` |
//!
//! The decoder reads **one flat object whose values are strings or
//! `null`**, plus arrays of strings for `start_session`'s `cmd` and
//! `env` and nowhere else. A number, a boolean or a nested object is a
//! protocol error, not something to be tolerant about. Keys the message
//! does not use are skipped (greetd may grow a field), but their values
//! must still be one of those shapes. See `docs/greeter.md`, decision 3.

use std::fmt;

/// The largest frame either side accepts. Every other decoder in the
/// tree bounds its input; a greeter blocked allocating 4 GiB is a login
/// screen that does not come up.
pub const MAX_FRAME: usize = 64 * 1024;

/// A message to the authenticator (greetd, or `nitro-auth`).
#[derive(Clone, PartialEq, Eq)]
pub enum Request {
    /// Start a conversation for `username`.
    CreateSession {
        /// Who is logging in.
        username: String,
    },
    /// Answer the last `auth_message`: the text typed, or `None` for an
    /// `info`/`error` line, which still needs a reply.
    PostAuthMessageResponse {
        /// What the user typed, if the message asked for anything.
        response: Option<String>,
    },
    /// After `success`: run `cmd` with `env` (`KEY=VALUE`) as the user.
    StartSession {
        /// The session's command line.
        cmd: Vec<String>,
        /// Extra environment, `KEY=VALUE`.
        env: Vec<String>,
    },
    /// Abandon the conversation in flight.
    CancelSession,
}

/// `Debug` never prints a response: it is usually a password.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateSession { username } => f
                .debug_struct("CreateSession")
                .field("username", username)
                .finish(),
            Self::PostAuthMessageResponse { response } => f
                .debug_struct("PostAuthMessageResponse")
                .field("response", &response.as_ref().map(|_| "<redacted>"))
                .finish(),
            Self::StartSession { cmd, env } => f
                .debug_struct("StartSession")
                .field("cmd", cmd)
                .field("env", env)
                .finish(),
            Self::CancelSession => f.write_str("CancelSession"),
        }
    }
}

/// Which kind of failure an `error` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Authentication failed: a wrong password, an unknown user.
    AuthError,
    /// Anything else: a protocol misuse, a broken PAM stack.
    Error,
}

/// What an `auth_message` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// A question whose answer may be shown (a user name, a code).
    Visible,
    /// A question whose answer must be masked (a password).
    Secret,
    /// A line to show; answered with `null`.
    Info,
    /// An error line to show; answered with `null`.
    Error,
}

/// A message from the authenticator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// The last request succeeded: authenticated, cancelled, started.
    Success,
    /// The last request failed.
    Error {
        /// Authentication failure or other error.
        kind: ErrorKind,
        /// Human-readable, from PAM or the authenticator.
        description: String,
    },
    /// PAM says something, and waits for `post_auth_message_response`.
    AuthMessage {
        /// What it asks for.
        kind: MessageKind,
        /// The prompt or the line, from PAM, localised.
        text: String,
    },
}

/// A malformed frame or message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "login protocol: {}", self.0)
    }
}

impl std::error::Error for ProtocolError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ProtocolError> {
    Err(ProtocolError(msg.into()))
}

// -- encoding -----------------------------------------------------------

/// Append `s` as a JSON string literal: `"` and `\` escaped, control
/// characters as `\u00XX`. This is what keeps a `"` in a password from
/// ending the string.
fn push_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for c in s.chars() {
        match c {
            '"' => out.extend_from_slice(b"\\\""),
            '\\' => out.extend_from_slice(b"\\\\"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.extend_from_slice(format!("\\u{:04x}", c as u32).as_bytes());
            }
            c => {
                let mut b = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
    out.push(b'"');
}

fn push_key(out: &mut Vec<u8>, key: &str) {
    out.push(b',');
    push_str(out, key);
    out.push(b':');
}

fn push_list(out: &mut Vec<u8>, items: &[String]) {
    out.push(b'[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_str(out, s);
    }
    out.push(b']');
}

/// Prefix `body` with its native-endian length.
fn frame(body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_ne_bytes());
    out.extend_from_slice(body);
    out
}

/// A request's JSON body, unframed.
#[must_use]
pub fn request_json(req: &Request) -> Vec<u8> {
    let mut o = Vec::with_capacity(64);
    o.extend_from_slice(b"{\"type\":");
    match req {
        Request::CreateSession { username } => {
            push_str(&mut o, "create_session");
            push_key(&mut o, "username");
            push_str(&mut o, username);
        }
        Request::PostAuthMessageResponse { response } => {
            push_str(&mut o, "post_auth_message_response");
            push_key(&mut o, "response");
            match response {
                Some(r) => push_str(&mut o, r),
                None => o.extend_from_slice(b"null"),
            }
        }
        Request::StartSession { cmd, env } => {
            push_str(&mut o, "start_session");
            push_key(&mut o, "cmd");
            push_list(&mut o, cmd);
            push_key(&mut o, "env");
            push_list(&mut o, env);
        }
        Request::CancelSession => push_str(&mut o, "cancel_session"),
    }
    o.push(b'}');
    o
}

/// A request as one frame, ready to write.
///
/// The caller should [`wipe`] the result once written: a
/// `post_auth_message_response` carries what the user typed.
#[must_use]
pub fn encode_request(req: &Request) -> Vec<u8> {
    let mut body = request_json(req);
    let out = frame(&body);
    wipe(&mut body);
    out
}

/// A response's JSON body, unframed.
#[must_use]
pub fn response_json(resp: &Response) -> Vec<u8> {
    let mut o = Vec::with_capacity(64);
    o.extend_from_slice(b"{\"type\":");
    match resp {
        Response::Success => push_str(&mut o, "success"),
        Response::Error { kind, description } => {
            push_str(&mut o, "error");
            push_key(&mut o, "error_type");
            push_str(
                &mut o,
                match kind {
                    ErrorKind::AuthError => "auth_error",
                    ErrorKind::Error => "error",
                },
            );
            push_key(&mut o, "description");
            push_str(&mut o, description);
        }
        Response::AuthMessage { kind, text } => {
            push_str(&mut o, "auth_message");
            push_key(&mut o, "auth_message_type");
            push_str(
                &mut o,
                match kind {
                    MessageKind::Visible => "visible",
                    MessageKind::Secret => "secret",
                    MessageKind::Info => "info",
                    MessageKind::Error => "error",
                },
            );
            push_key(&mut o, "auth_message");
            push_str(&mut o, text);
        }
    }
    o.push(b'}');
    o
}

/// A response as one frame, ready to write.
#[must_use]
pub fn encode_response(resp: &Response) -> Vec<u8> {
    frame(&response_json(resp))
}

// -- decoding -----------------------------------------------------------

/// A value in a flat object: the only three shapes the protocol has.
#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    Null,
    List(Vec<String>),
}

/// One flat JSON object, as its fields in order.
struct Object(Vec<(String, Value)>);

impl Object {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn string(&self, key: &str) -> Result<String, ProtocolError> {
        match self.get(key) {
            Some(Value::Str(s)) => Ok(s.clone()),
            Some(_) => err(format!("`{key}` is not a string")),
            None => err(format!("missing `{key}`")),
        }
    }

    fn opt_string(&self, key: &str) -> Result<Option<String>, ProtocolError> {
        match self.get(key) {
            Some(Value::Str(s)) => Ok(Some(s.clone())),
            Some(Value::Null) => Ok(None),
            Some(Value::List(_)) => err(format!("`{key}` is a list")),
            None => err(format!("missing `{key}`")),
        }
    }

    fn list(&self, key: &str) -> Result<Vec<String>, ProtocolError> {
        match self.get(key) {
            Some(Value::List(l)) => Ok(l.clone()),
            Some(_) => err(format!("`{key}` is not a list")),
            None => err(format!("missing `{key}`")),
        }
    }

    /// Refuse a list anywhere but under `allowed`.
    fn lists_only_in(&self, allowed: &[&str]) -> Result<(), ProtocolError> {
        for (k, v) in &self.0 {
            if matches!(v, Value::List(_)) && !allowed.contains(&k.as_str()) {
                return err(format!("`{k}` may not be a list"));
            }
        }
        Ok(())
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        for (_, v) in &mut self.0 {
            if let Value::Str(s) = v {
                wipe_string(s);
            }
        }
    }
}

/// A cursor over one JSON text.
struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.at) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.at).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), ProtocolError> {
        if self.peek() == Some(c) {
            self.at += 1;
            Ok(())
        } else {
            err(format!("expected `{}` at byte {}", c as char, self.at))
        }
    }

    fn object(&mut self) -> Result<Object, ProtocolError> {
        self.eat(b'{')?;
        let mut fields = Object(Vec::new());
        if self.peek() == Some(b'}') {
            self.at += 1;
        } else {
            loop {
                if self.peek() != Some(b'"') {
                    return err("expected a key");
                }
                let key = self.string()?;
                if fields.get(&key).is_some() {
                    return err(format!("duplicate key `{key}`"));
                }
                self.eat(b':')?;
                let value = self.value()?;
                fields.0.push((key, value));
                match self.peek() {
                    Some(b',') => self.at += 1,
                    Some(b'}') => {
                        self.at += 1;
                        break;
                    }
                    _ => return err("expected `,` or `}`"),
                }
            }
        }
        if self.peek().is_some() {
            return err("trailing bytes after the object");
        }
        Ok(fields)
    }

    fn value(&mut self) -> Result<Value, ProtocolError> {
        match self.peek() {
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b'n') if self.b[self.at..].starts_with(b"null") => {
                self.at += 4;
                Ok(Value::Null)
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::List(items));
                }
                loop {
                    if self.peek() != Some(b'"') {
                        return err("a list may hold only strings");
                    }
                    items.push(self.string()?);
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::List(items));
                        }
                        _ => return err("expected `,` or `]`"),
                    }
                }
            }
            _ => err(format!(
                "byte {}: only strings and null are values here",
                self.at
            )),
        }
    }

    fn hex4(&mut self) -> Result<u32, ProtocolError> {
        let Some(digits) = self.b.get(self.at..self.at + 4) else {
            return err("truncated `\\u` escape");
        };
        let mut v = 0;
        for &d in digits {
            let n = match d {
                b'0'..=b'9' => d - b'0',
                b'a'..=b'f' => d - b'a' + 10,
                b'A'..=b'F' => d - b'A' + 10,
                _ => return err("bad hex digit in `\\u` escape"),
            };
            v = v * 16 + u32::from(n);
        }
        self.at += 4;
        Ok(v)
    }

    /// A string literal, the cursor on its opening quote.
    fn string(&mut self) -> Result<String, ProtocolError> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.b.get(self.at) else {
                wipe(&mut out);
                return err("unterminated string");
            };
            self.at += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let Some(&e) = self.b.get(self.at) else {
                        wipe(&mut out);
                        return err("unterminated escape");
                    };
                    self.at += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => match self.unicode() {
                            Ok(ch) => ch,
                            Err(e) => {
                                wipe(&mut out);
                                return Err(e);
                            }
                        },
                        _ => {
                            wipe(&mut out);
                            return err("bad escape");
                        }
                    };
                    let mut b = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                }
                c if c < 0x20 => {
                    wipe(&mut out);
                    return err("raw control character in a string");
                }
                c => out.push(c),
            }
        }
        String::from_utf8(out).or_else(|e| {
            let mut v = e.into_bytes();
            wipe(&mut v);
            err("a string is not UTF-8")
        })
    }

    /// The code point of a `\uXXXX` escape (the `\u` already read),
    /// joining a surrogate pair.
    fn unicode(&mut self) -> Result<char, ProtocolError> {
        let hi = self.hex4()?;
        let cp = if (0xD800..0xDC00).contains(&hi) {
            if self.b.get(self.at..self.at + 2) != Some(b"\\u") {
                return err("lone high surrogate");
            }
            self.at += 2;
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return err("high surrogate without a low one");
            }
            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
        } else if (0xDC00..0xE000).contains(&hi) {
            return err("lone low surrogate");
        } else {
            hi
        };
        char::from_u32(cp).map_or_else(|| err("bad code point"), Ok)
    }
}

fn parse(body: &[u8]) -> Result<(String, Object), ProtocolError> {
    let obj = Parser { b: body, at: 0 }.object()?;
    let ty = obj.string("type")?;
    Ok((ty, obj))
}

/// Decode a request from one frame's JSON body.
///
/// # Errors
/// Anything that is not exactly one of the four requests.
pub fn decode_request(body: &[u8]) -> Result<Request, ProtocolError> {
    let (ty, o) = parse(body)?;
    let allowed: &[&str] = if ty == "start_session" {
        &["cmd", "env"]
    } else {
        &[]
    };
    o.lists_only_in(allowed)?;
    match ty.as_str() {
        "create_session" => Ok(Request::CreateSession {
            username: o.string("username")?,
        }),
        "post_auth_message_response" => Ok(Request::PostAuthMessageResponse {
            response: o.opt_string("response")?,
        }),
        "start_session" => Ok(Request::StartSession {
            cmd: o.list("cmd")?,
            env: match o.get("env") {
                None => Vec::new(),
                Some(_) => o.list("env")?,
            },
        }),
        "cancel_session" => Ok(Request::CancelSession),
        other => err(format!("unknown request `{other}`")),
    }
}

/// Decode a response from one frame's JSON body.
///
/// # Errors
/// Anything that is not exactly one of the three responses.
pub fn decode_response(body: &[u8]) -> Result<Response, ProtocolError> {
    let (ty, o) = parse(body)?;
    o.lists_only_in(&[])?;
    match ty.as_str() {
        "success" => Ok(Response::Success),
        "error" => Ok(Response::Error {
            kind: match o.string("error_type")?.as_str() {
                "auth_error" => ErrorKind::AuthError,
                "error" => ErrorKind::Error,
                other => return err(format!("unknown error_type `{other}`")),
            },
            description: o.string("description")?,
        }),
        "auth_message" => Ok(Response::AuthMessage {
            kind: match o.string("auth_message_type")?.as_str() {
                "visible" => MessageKind::Visible,
                "secret" => MessageKind::Secret,
                "info" => MessageKind::Info,
                "error" => MessageKind::Error,
                other => return err(format!("unknown auth_message_type `{other}`")),
            },
            text: o.string("auth_message")?,
        }),
        other => err(format!("unknown response `{other}`")),
    }
}

// -- framing ------------------------------------------------------------

/// Reassembles frames from partial reads.
///
/// Push whatever a non-blocking `read` returned; pop complete frames.
/// A length over [`MAX_FRAME`] is an error, raised as soon as the four
/// length bytes are in, before anything is allocated for the body.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    /// An empty reassembly buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes as they arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Bytes held that do not make a whole frame yet.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// The next complete frame's body, if one is in.
    ///
    /// The consumed bytes are zeroed before they are dropped from the
    /// buffer: a frame may have carried a password.
    ///
    /// # Errors
    /// A frame longer than [`MAX_FRAME`]. The stream cannot be resynced
    /// after that; the caller should drop the connection.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let Some(head) = self.buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_ne_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if len > MAX_FRAME {
            return err(format!("frame of {len} bytes (the limit is {MAX_FRAME})"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let body = self.buf[4..4 + len].to_vec();
        for b in &mut self.buf[..4 + len] {
            *b = 0;
        }
        std::hint::black_box(&mut self.buf);
        self.buf.drain(..4 + len);
        Ok(Some(body))
    }
}

impl Drop for Framer {
    fn drop(&mut self) {
        wipe(&mut self.buf);
    }
}

/// Zero a buffer that carried a secret, including its spare capacity.
///
/// Best effort, as the secret `TextField`'s own wipe is (`docs/ui.md`):
/// `black_box` rather than `write_volatile`, which is `unsafe`. Hygiene,
/// not a boundary.
pub fn wipe(bytes: &mut Vec<u8>) {
    let cap = bytes.capacity();
    bytes.clear();
    bytes.resize(cap, 0);
    std::hint::black_box(&mut *bytes);
    bytes.clear();
}

/// [`wipe`] for a `String`.
pub fn wipe_string(s: &mut String) {
    let mut v = std::mem::take(s).into_bytes();
    wipe(&mut v);
    // Hand the (zeroed, empty) allocation back, so the string's buffer
    // is not freed with a secret in it by whoever drops it next.
    *s = String::from_utf8(v).unwrap_or_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(frame: &[u8]) -> &[u8] {
        let len = u32::from_ne_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(frame.len(), 4 + len);
        &frame[4..]
    }

    fn requests() -> Vec<Request> {
        vec![
            Request::CreateSession {
                username: "alice".into(),
            },
            Request::PostAuthMessageResponse {
                response: Some("hunter2".into()),
            },
            Request::PostAuthMessageResponse { response: None },
            Request::StartSession {
                cmd: vec!["nitro-session".into(), "--x".into()],
                env: vec!["A=1".into()],
            },
            Request::StartSession {
                cmd: vec![],
                env: vec![],
            },
            Request::CancelSession,
        ]
    }

    fn responses() -> Vec<Response> {
        let mut v = vec![Response::Success];
        for kind in [ErrorKind::AuthError, ErrorKind::Error] {
            v.push(Response::Error {
                kind,
                description: "nope".into(),
            });
        }
        for kind in [
            MessageKind::Visible,
            MessageKind::Secret,
            MessageKind::Info,
            MessageKind::Error,
        ] {
            v.push(Response::AuthMessage {
                kind,
                text: "Password: ".into(),
            });
        }
        v
    }

    #[test]
    fn every_request_round_trips() {
        for r in requests() {
            let f = encode_request(&r);
            assert_eq!(decode_request(body(&f)).unwrap(), r, "{r:?}");
        }
    }

    #[test]
    fn every_response_round_trips() {
        for r in responses() {
            let f = encode_response(&r);
            assert_eq!(decode_response(body(&f)).unwrap(), r, "{r:?}");
        }
    }

    #[test]
    fn the_wire_form_is_greetds() {
        assert_eq!(
            request_json(&Request::CreateSession {
                username: "al".into()
            }),
            br#"{"type":"create_session","username":"al"}"#
        );
        assert_eq!(
            request_json(&Request::PostAuthMessageResponse { response: None }),
            br#"{"type":"post_auth_message_response","response":null}"#
        );
        assert_eq!(
            response_json(&Response::AuthMessage {
                kind: MessageKind::Secret,
                text: "Password:".into()
            }),
            br#"{"type":"auth_message","auth_message_type":"secret","auth_message":"Password:"}"#
        );
        // greetd's own output: key order and whitespace are not ours.
        assert_eq!(
            decode_response(
                br#" { "description" : "x", "error_type":"auth_error", "type":"error" } "#
            )
            .unwrap(),
            Response::Error {
                kind: ErrorKind::AuthError,
                description: "x".into()
            }
        );
    }

    #[test]
    fn awkward_passwords_survive_the_escaper() {
        for pw in [
            "a\"b",
            "back\\slash",
            "\\\"",
            "new\nline\ttab\r",
            "\u{1}\u{1f}\u{7f}",
            "grüße ünïcödé",
            "emoji 🔑 pair",
            "",
            "\"}, {\"type\":\"cancel_session\"}",
        ] {
            let r = Request::PostAuthMessageResponse {
                response: Some(pw.into()),
            };
            let json = request_json(&r);
            assert!(
                !json.iter().any(|&b| b < 0x20),
                "raw control byte in {json:?}"
            );
            assert_eq!(decode_request(&json).unwrap(), r, "{pw:?}");
        }
    }

    #[test]
    fn escapes_decode_including_surrogate_pairs() {
        let r = decode_response(
            br#"{"type":"auth_message","auth_message_type":"info","auth_message":"\u00e9\ud83d\udd11\n\/\"\\\b\f\r\t"}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Response::AuthMessage {
                kind: MessageKind::Info,
                text: "é🔑\n/\"\\\u{8}\u{c}\r\t".into()
            }
        );
    }

    #[test]
    fn malformed_input_is_refused() {
        for bad in [
            &br#"{"type":"success","n":1}"#[..],
            br#"{"type":"success","o":{}}"#,
            br#"{"type":"success","b":true}"#,
            br#"{"type":"success","l":["a"]}"#,
            br#"{"username":"a"}"#,
            br#"{"type":"frobnicate"}"#,
            br#"{"type":null}"#,
            br#"{"type":"success"} x"#,
            br#"{"type":"success"}{}"#,
            br#"{"type":"success"#,
            br#"{"type":"su\qccess"}"#,
            br#"{"type":"\ud800"}"#,
            br#"{"type":"\udc00"}"#,
            br#"{"type":"\ud800\u0041"}"#,
            br#"{"type":"\u12"}"#,
            br#"{"type":"success","type":"success"}"#,
            b"{\"type\":\"succ\x01ess\"}",
            b"{\"type\":\"\xff\"}",
            br#"{"type":"error","error_type":"bad","description":"x"}"#,
            br#"{"type":"error","description":"x"}"#,
            br#"{"type":"auth_message","auth_message_type":"secret"}"#,
            br#"["type"]"#,
            b"",
        ] {
            assert!(
                decode_response(bad).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(bad)
            );
        }
        for bad in [
            &br#"{"type":"create_session"}"#[..],
            br#"{"type":"create_session","username":null}"#,
            br#"{"type":"create_session","username":["a"]}"#,
            br#"{"type":"post_auth_message_response"}"#,
            br#"{"type":"post_auth_message_response","response":3}"#,
            br#"{"type":"start_session","cmd":"sh"}"#,
            br#"{"type":"start_session","cmd":[1]}"#,
            br#"{"type":"start_session","cmd":[["a"]]}"#,
            br#"{"type":"cancel_session","x":[]}"#,
        ] {
            assert!(
                decode_request(bad).is_err(),
                "accepted {:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn unknown_keys_are_skipped_but_their_values_still_checked() {
        assert_eq!(
            decode_response(br#"{"type":"success","extra":"x","more":null}"#).unwrap(),
            Response::Success
        );
    }

    #[test]
    fn frames_reassemble_across_split_reads() {
        let mut stream = Vec::new();
        for r in responses() {
            stream.extend(encode_response(&r));
        }
        for chunk in [1, 2, 3, 5, 7, 64, stream.len()] {
            let mut f = Framer::new();
            let mut got = Vec::new();
            for piece in stream.chunks(chunk) {
                f.push(piece);
                while let Some(b) = f.next_frame().unwrap() {
                    got.push(decode_response(&b).unwrap());
                }
            }
            assert_eq!(got, responses(), "chunk {chunk}");
            assert_eq!(f.pending(), 0);
        }
    }

    #[test]
    fn an_over_long_frame_is_refused_before_its_body_arrives() {
        let mut f = Framer::new();
        f.push(&u32::try_from(MAX_FRAME + 1).unwrap().to_ne_bytes());
        assert!(f.next_frame().is_err());
        let mut f = Framer::new();
        f.push(&u32::MAX.to_ne_bytes());
        assert!(f.next_frame().is_err());
        // Exactly the limit is fine.
        let mut f = Framer::new();
        f.push(&u32::try_from(MAX_FRAME).unwrap().to_ne_bytes());
        f.push(&vec![b' '; MAX_FRAME]);
        assert_eq!(f.next_frame().unwrap().unwrap().len(), MAX_FRAME);
    }

    #[test]
    fn debug_never_prints_a_response() {
        let r = Request::PostAuthMessageResponse {
            response: Some("hunter2".into()),
        };
        assert!(!format!("{r:?}").contains("hunter2"));
    }

    #[test]
    fn wipe_leaves_an_empty_buffer_with_its_capacity() {
        let mut v = b"secret".to_vec();
        let cap = v.capacity();
        wipe(&mut v);
        assert!(v.is_empty());
        assert_eq!(v.capacity(), cap);
        let mut s = String::from("secret");
        wipe_string(&mut s);
        assert!(s.is_empty());
    }
}
