//! The real [`Authenticator`]: one PAM transaction per attempt, through
//! `nonstick`.
//!
//! No `unsafe` here: the FFI is inside `nonstick`/`libpam-sys`, the way
//! `xkbcommon`'s is inside its crate (`DEPENDENCIES.md`).

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt as _;

use nitro_login::MessageKind;
use nonstick::conv::Exchange;
use nonstick::items::Items as _;
use nonstick::{
    AuthnFlags, Conversation, ErrorCode, PamShared as _, Transaction as _, TransactionBuilder,
};

use crate::serve::{Authenticator, Failure, Prompter};

/// The PAM service `nitro-auth` uses unless told otherwise:
/// `/etc/pam.d/nitro-lock` (`deploy/pam.d/nitro-lock`).
pub const DEFAULT_SERVICE: &str = "nitro-lock";

/// PAM, for a named service.
#[derive(Debug, Clone)]
pub struct Pam {
    service: String,
}

impl Pam {
    /// PAM with the stack in `/etc/pam.d/<service>`.
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

/// PAM's conversation callback, onto the [`Prompter`].
///
/// `Conversation::communicate` takes `&self`, hence the `RefCell`. PAM
/// calls it from inside `pam_authenticate`, on this thread, never
/// re-entrantly.
struct Bridge<'a> {
    prompter: RefCell<&'a mut dyn Prompter>,
}

fn text(q: &std::ffi::OsStr) -> String {
    q.to_string_lossy().into_owned()
}

impl Conversation for Bridge<'_> {
    fn communicate(&self, messages: &[Exchange]) {
        let mut p = self.prompter.borrow_mut();
        for m in messages {
            match m {
                Exchange::MaskedPrompt(q) => q.set_answer(
                    p.ask(MessageKind::Secret, &text(q.question()))
                        .map(|a| OsString::from_vec(a.unwrap_or_default().into_bytes()))
                        .map_err(|_| ErrorCode::ConversationError),
                ),
                Exchange::Prompt(q) => q.set_answer(
                    p.ask(MessageKind::Visible, &text(q.question()))
                        .map(|a| OsString::from_vec(a.unwrap_or_default().into_bytes()))
                        .map_err(|_| ErrorCode::ConversationError),
                ),
                Exchange::Info(q) => q.set_answer(
                    p.ask(MessageKind::Info, &text(q.question()))
                        .map(|_| ())
                        .map_err(|_| ErrorCode::ConversationError),
                ),
                Exchange::Error(q) => q.set_answer(
                    p.ask(MessageKind::Error, &text(q.question()))
                        .map(|_| ())
                        .map_err(|_| ErrorCode::ConversationError),
                ),
                // Radio and binary prompts (Linux-PAM extensions) have no
                // rendering in greetd's protocol either.
                other => other.set_error(ErrorCode::ConversationError),
            }
        }
    }
}

/// Which failures mean "not you" rather than "broken".
fn failure(e: ErrorCode) -> Failure {
    let text = e.to_string();
    match e {
        ErrorCode::AuthenticationError
        | ErrorCode::MaxTries
        | ErrorCode::UserUnknown
        | ErrorCode::CredentialsInsufficient
        | ErrorCode::AuthInfoUnavailable
        | ErrorCode::PermissionDenied
        | ErrorCode::AccountExpired
        | ErrorCode::NewAuthTokRequired
        | ErrorCode::AuthTokExpired => Failure::Auth(text),
        _ => Failure::Other(text),
    }
}

impl Authenticator for Pam {
    fn authenticate(&mut self, user: &str, conv: &mut dyn Prompter) -> Result<(), Failure> {
        let bridge = Bridge {
            prompter: RefCell::new(conv),
        };
        let mut tx = TransactionBuilder::new_with_service(&self.service)
            .username(user)
            .build(bridge)
            .map_err(|e| Failure::Other(format!("pam_start({}): {e}", self.service)))?;
        tx.authenticate(AuthnFlags::DISALLOW_NULL_AUTHTOK)
            .map_err(failure)?;
        tx.account_management(AuthnFlags::DISALLOW_NULL_AUTHTOK)
            .map_err(failure)?;
        // A module may rewrite PAM_USER. What was authenticated must
        // still be the user who asked, or it unlocks nothing.
        match tx.items().user() {
            Ok(Some(u)) if u == std::ffi::OsStr::new(user) => Ok(()),
            Ok(None) => Ok(()),
            Ok(Some(u)) => Err(Failure::Other(format!(
                "PAM authenticated {} instead",
                u.to_string_lossy()
            ))),
            Err(e) => Err(Failure::Other(format!("PAM_USER: {e}"))),
        }
        // No `pam_setcred`: nonstick has no binding for it yet, so an
        // unlock does not refresh credentials (Kerberos tickets). Fine
        // for a lock screen; recorded in the README.
    }
}
