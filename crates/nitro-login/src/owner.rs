//! Who owns this session.

/// The session's user: `$USER`, then `$LOGNAME`, if non-empty; else the
/// `/etc/passwd` entry for `getuid()`.
///
/// Both sides of the lock screen ask: `nitro-greeter` to prefill the
/// name, `nitro-auth` to refuse anyone else. The environment comes first
/// because a login sets it and it is what the user's own tools believe;
/// the helper is spawned by the session, not by a stranger, so trusting
/// it is no weaker than trusting the process that asks.
#[must_use]
pub fn owner() -> Option<String> {
    for var in ["USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return Some(v);
        }
    }
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    name_for_uid(&passwd, rustix::process::getuid().as_raw())
}

/// The name of `uid` in a `passwd` file's text.
#[must_use]
pub fn name_for_uid(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let mut f = line.split(':');
        let name = f.next()?;
        let _pw = f.next()?;
        let id: u32 = f.next()?.parse().ok()?;
        (id == uid && !name.is_empty()).then(|| name.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uid_is_found_in_passwd() {
        let p = "root:x:0:0::/root:/bin/sh\n# junk\nalice:x:1000:1000::/home/alice:/bin/sh\n";
        assert_eq!(name_for_uid(p, 1000).as_deref(), Some("alice"));
        assert_eq!(name_for_uid(p, 0).as_deref(), Some("root"));
        assert_eq!(name_for_uid(p, 7), None);
    }

    #[test]
    fn someone_owns_the_test_process() {
        assert!(owner().is_some_and(|o| !o.is_empty()));
    }
}
