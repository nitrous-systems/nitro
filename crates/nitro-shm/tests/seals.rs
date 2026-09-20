//! Real-kernel tests for the seals and the mapping.
//!
//! Not runnable under Miri (no shims for `memfd_create`, `F_ADD_SEALS`,
//! `F_GET_SEALS` or a file-backed `mmap`, and rustix's `linux_raw` backend
//! uses inline asm), which is why each safety-relevant claim in
//! `src/map.rs` is pinned here against what the kernel actually does.

use std::os::fd::{AsFd, OwnedFd};

use nitro_shm::{
    Errno, MapError, Mapping, MappingMut, REQUIRED_SEALS, SealError, check_seals, create_sealed,
    memfd_with, sealed_len,
};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};

const LEN: usize = 4096 * 3 + 100;

/// A sealable memfd of `LEN` bytes with exactly `seals` applied.
fn memfd_sealed_with(seals: SealFlags) -> OwnedFd {
    let fd = memfd_create(
        "nitro-shm-test",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .unwrap();
    ftruncate(&fd, LEN as u64).unwrap();
    if !seals.is_empty() {
        fcntl_add_seals(&fd, seals).unwrap();
    }
    fd
}

fn pwrite_all(fd: &OwnedFd, data: &[u8], mut off: u64) {
    let mut done = 0;
    while done < data.len() {
        let n = rustix::io::pwrite(fd, &data[done..], off).unwrap();
        assert!(n > 0);
        done += n;
        off += n as u64;
    }
}

fn pread_all(fd: &OwnedFd, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = rustix::io::pread(fd, &mut buf[done..], done as u64).unwrap();
        assert!(n > 0, "short read");
        done += n;
    }
    buf
}

/// Random-looking bytes, reproducible.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed & 0xFF) as u8
        })
        .collect()
}

/// Lines of `/proc/self/maps` mentioning a memfd of this name.
fn mapped_count(name: &str) -> usize {
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    maps.lines()
        .filter(|l| l.contains(&format!("/memfd:{name}")))
        .count()
}

// ------------------------------------------------------------- per-seal

#[test]
fn a_fully_sealed_memfd_passes() {
    let fd = create_sealed("nitro-shm-ok", LEN as u64).unwrap();
    check_seals(fd.as_fd()).unwrap();
    assert_eq!(sealed_len(&fd).unwrap(), LEN as u64);
}

#[test]
fn a_plain_memfd_without_allow_sealing_is_refused() {
    // Without `MFD_ALLOW_SEALING` the kernel reports only `F_SEAL_SEAL`,
    // so a legacy client that never heard of sealing is refused on the
    // two seals that matter.
    let fd = memfd_create("nitro-shm-plain", MemfdFlags::CLOEXEC).unwrap();
    ftruncate(&fd, LEN as u64).unwrap();
    let err = check_seals(fd.as_fd()).unwrap_err();
    assert_eq!(err, SealError::Missing(SealFlags::SHRINK | SealFlags::GROW));
    assert_eq!(err.to_string(), "fd lacks F_SEAL_SHRINK, F_SEAL_GROW");
}

#[test]
fn missing_shrink_is_refused() {
    let fd = memfd_sealed_with(SealFlags::GROW | SealFlags::SEAL);
    assert_eq!(
        check_seals(fd.as_fd()),
        Err(SealError::Missing(SealFlags::SHRINK))
    );
    assert!(matches!(
        Mapping::map(fd, LEN),
        Err(MapError::Seals(SealError::Missing(m))) if m == SealFlags::SHRINK
    ));
}

#[test]
fn missing_grow_is_refused() {
    let fd = memfd_sealed_with(SealFlags::SHRINK | SealFlags::SEAL);
    assert_eq!(
        check_seals(fd.as_fd()),
        Err(SealError::Missing(SealFlags::GROW))
    );
    assert!(matches!(
        Mapping::map(fd, LEN),
        Err(MapError::Seals(SealError::Missing(m))) if m == SealFlags::GROW
    ));
}

#[test]
fn missing_seal_seal_is_refused() {
    let fd = memfd_sealed_with(SealFlags::SHRINK | SealFlags::GROW);
    assert_eq!(
        check_seals(fd.as_fd()),
        Err(SealError::Missing(SealFlags::SEAL))
    );
    assert!(matches!(
        Mapping::map(fd, LEN),
        Err(MapError::Seals(SealError::Missing(m))) if m == SealFlags::SEAL
    ));
}

#[test]
fn an_entirely_unsealed_memfd_is_refused_naming_all_three() {
    let fd = memfd_sealed_with(SealFlags::empty());
    let err = check_seals(fd.as_fd()).unwrap_err();
    assert_eq!(err, SealError::Missing(REQUIRED_SEALS));
    assert_eq!(
        err.to_string(),
        "fd lacks F_SEAL_SHRINK, F_SEAL_GROW, F_SEAL_SEAL"
    );
}

#[test]
fn a_regular_file_is_unsealable() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("nitro-shm-regular-{}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(LEN as u64).unwrap();
    let fd = OwnedFd::from(file);
    let err = check_seals(fd.as_fd()).unwrap_err();
    assert!(matches!(err, SealError::Unsealable(_)), "{err}");
    assert!(matches!(
        Mapping::map(fd, LEN),
        Err(MapError::Seals(SealError::Unsealable(_)))
    ));
    let _ = std::fs::remove_file(path);
}

// ------------------------------------------------------------- hostile

/// The positive proof the seal is *in force*, not merely reported: the
/// kernel refuses the shrink with `EPERM`.
#[test]
fn a_sealed_memfd_cannot_be_shrunk() {
    let fd = create_sealed("nitro-shm-hostile", LEN as u64).unwrap();
    let map = Mapping::map(fd.try_clone().unwrap(), LEN).unwrap();
    assert_eq!(ftruncate(&fd, 0), Err(Errno::PERM));
    assert_eq!(ftruncate(&fd, (LEN - 1) as u64), Err(Errno::PERM));
    // And the mapping is still fully readable afterwards — every byte,
    // including the last page, which is the one a shrink would have
    // turned into a SIGBUS.
    assert_eq!(map.as_bytes().len(), LEN);
    assert_eq!(map.as_bytes()[LEN - 1], 0);
}

#[test]
fn a_sealed_memfd_cannot_be_grown() {
    let fd = create_sealed("nitro-shm-hostile", LEN as u64).unwrap();
    assert_eq!(ftruncate(&fd, (LEN * 2) as u64), Err(Errno::PERM));
    assert_eq!(sealed_len(&fd).unwrap(), LEN as u64);
}

#[test]
fn a_sealed_memfd_cannot_gain_seals() {
    // `F_SEAL_SEAL` in force: a client cannot add `F_SEAL_WRITE` after the
    // fact and change the contract the server checked.
    let fd = create_sealed("nitro-shm-hostile", LEN as u64).unwrap();
    assert_eq!(fcntl_add_seals(&fd, SealFlags::WRITE), Err(Errno::PERM));
    assert_eq!(
        fcntl_add_seals(&fd, SealFlags::FUTURE_WRITE),
        Err(Errno::PERM)
    );
}

/// The seals fix the size, not the contents: writing stays allowed, which
/// is the whole point of the buffer.
#[test]
fn a_sealed_memfd_is_still_writable() {
    let fd = create_sealed("nitro-shm-writable", LEN as u64).unwrap();
    pwrite_all(&fd, &[0xAB; 16], 0);
    assert_eq!(pread_all(&fd, 16), vec![0xAB; 16]);
}

// ------------------------------------------------------------- mapping

#[test]
fn a_too_short_file_is_refused_before_mapping() {
    let fd = create_sealed("nitro-shm-short", 100).unwrap();
    assert_eq!(
        Mapping::map(fd, 101).map(|_| ()),
        Err(MapError::TooShort {
            file: 100,
            need: 101
        })
    );
}

#[test]
fn a_zero_length_mapping_is_refused() {
    let fd = create_sealed("nitro-shm-zero", 100).unwrap();
    assert!(matches!(
        Mapping::map(fd, 0),
        Err(MapError::Os(Errno::INVAL))
    ));
}

/// The golden: the mapped bytes are the `pread` bytes, byte for byte, and
/// a write *after* mapping is visible through it with no re-read.
#[test]
fn a_mapping_sees_the_file_byte_for_byte_and_live() {
    let bytes = noise(LEN, 0x9E37_79B9_7F4A_7C15);
    let fd = memfd_with("nitro-shm-golden", &bytes).unwrap();
    let copy = pread_all(&fd, LEN);
    assert_eq!(copy, bytes);

    let map = Mapping::map(fd.try_clone().unwrap(), LEN).unwrap();
    assert_eq!(map.as_bytes(), &bytes[..]);
    assert_eq!(map.len(), LEN);
    assert_eq!(map.file_len(), LEN as u64);

    // Live: a write through the client's descriptor shows up in the
    // server's mapping without any call on the mapping.
    let later = noise(LEN, 0xD1B5_4A32_D192_ED03);
    pwrite_all(&fd, &later, 0);
    assert_eq!(map.as_bytes(), &later[..]);
    assert_eq!(pread_all(&fd, LEN), later);
}

/// A shorter mapping of a longer file is fine, and sees only its prefix.
#[test]
fn a_prefix_can_be_mapped() {
    let bytes = noise(LEN, 7);
    let fd = memfd_with("nitro-shm-prefix", &bytes).unwrap();
    let map = Mapping::map(fd, 1000).unwrap();
    assert_eq!(map.as_bytes(), &bytes[..1000]);
    assert_eq!(map.file_len(), LEN as u64);
}

#[test]
fn the_fd_can_be_closed_once_mapped() {
    let bytes = noise(LEN, 3);
    let fd = memfd_with("nitro-shm-closed", &bytes).unwrap();
    // `Mapping::map` consumes and closes the only descriptor.
    let map = Mapping::map(fd, LEN).unwrap();
    assert_eq!(mapped_count("nitro-shm-closed"), 1);
    let links: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter(|t| t.to_string_lossy().contains("nitro-shm-closed"))
        .collect();
    assert!(links.is_empty(), "the fd is still open: {links:?}");
    // And the bytes are still there: the mapping pins the inode.
    assert_eq!(map.as_bytes(), &bytes[..]);
}

#[test]
fn dropping_a_mapping_unmaps_it() {
    let name = "nitro-shm-drop";
    let before = mapped_count(name);
    let fd = create_sealed(name, LEN as u64).unwrap();
    let map = Mapping::map(fd, LEN).unwrap();
    assert_eq!(mapped_count(name), before + 1);
    drop(map);
    assert_eq!(mapped_count(name), before);
}

#[test]
fn mapping_and_dropping_in_a_loop_does_not_accumulate() {
    let name = "nitro-shm-loop";
    let before = mapped_count(name);
    for _ in 0..64 {
        let fd = create_sealed(name, LEN as u64).unwrap();
        let map = Mapping::map(fd, LEN).unwrap();
        std::hint::black_box(map.as_bytes()[0]);
    }
    assert_eq!(mapped_count(name), before);
}

// ------------------------------------------------------------- writable

#[test]
fn a_writable_mapping_writes_straight_into_the_file() {
    let fd = create_sealed("nitro-shm-client", LEN as u64).unwrap();
    let mut mine = MappingMut::map_mut(fd.as_fd(), LEN).unwrap();
    assert_eq!(mine.len(), LEN);
    // The "server" maps the same file read-only through a dup — exactly
    // the arrangement `CreateBuffer` sets up.
    let theirs = Mapping::map(fd.try_clone().unwrap(), LEN).unwrap();

    let frame = noise(LEN, 11);
    mine.as_bytes_mut().copy_from_slice(&frame);
    assert_eq!(mine.as_bytes(), &frame[..]);
    assert_eq!(theirs.as_bytes(), &frame[..]);
    assert_eq!(pread_all(&fd, LEN), frame);

    // Closing the client's descriptor (as moving it into `CreateBuffer`
    // does) leaves its mapping usable.
    drop(fd);
    let next = noise(LEN, 12);
    mine.as_bytes_mut().copy_from_slice(&next);
    assert_eq!(theirs.as_bytes(), &next[..]);
}

#[test]
fn a_writable_mapping_needs_the_seals_too() {
    let fd = memfd_sealed_with(SealFlags::empty());
    assert!(matches!(
        MappingMut::map_mut(fd.as_fd(), LEN),
        Err(MapError::Seals(SealError::Missing(m))) if m == REQUIRED_SEALS
    ));
}

#[test]
fn the_required_seals_are_exactly_the_three() {
    assert_eq!(
        REQUIRED_SEALS,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::SEAL
    );
    assert!(!REQUIRED_SEALS.contains(SealFlags::WRITE));
}

/// The size of the audited surface, asserted rather than described — in
/// the code **and** in every document that quotes the number.
///
/// `DEPENDENCIES.md`, this crate's `README.md` and `map.rs`'s own header
/// all tell a reviewer how many `unsafe` blocks there are to audit. That
/// number is the one thing in the prose a reader cannot check at a glance
/// and is most likely to be wrong after a refactor — and a stale count is
/// worse than none, because it tells an auditor they have seen
/// everything when they have not.
///
/// It has now been wrong twice. The first draft said "three", having
/// counted `from_raw_parts` and `from_raw_parts_mut` as one; the fix
/// updated three places and **missed a fourth** (`DEPENDENCIES.md`'s
/// icons-comparison paragraph), which the reviewer of #569 then found by
/// hand. The first version of this test could not have caught that: it
/// counted blocks in `map.rs` and never read the documents. So it reads
/// them now — the whole point is that no human should have to diff a
/// number against four files again.
#[test]
fn the_unsafe_surface_is_exactly_what_the_docs_claim() {
    const BLOCKS: usize = 4;
    const WORD: &str = "four";
    /// The number words a doc might use, so a stale one is caught rather
    /// than merely "not found".
    const NUMBER_WORDS: [&str; 6] = ["one", "two", "three", "four", "five", "six"];

    let map = include_str!("../src/map.rs");
    let blocks = map.matches("unsafe {").count();
    assert_eq!(
        blocks, BLOCKS,
        "map.rs has {blocks} `unsafe` blocks; the docs (DEPENDENCIES.md, \
         README.md, map.rs's header) all say {WORD}. Update them together \
         with the code, or an auditor reads a count that no longer \
         describes what they must audit."
    );
    // And the two syscalls that must exist exactly once each, because
    // "one mmap and one munmap in the whole tree" is the claim that makes
    // the surface auditable in one sitting.
    assert_eq!(map.matches("rustix::mm::mmap(").count(), 1);
    assert_eq!(map.matches("rustix::mm::munmap(").count(), 1);

    // The other half of that claim: nothing *outside* this file maps or
    // unmaps. `nitro-kms` has its own DRM mappings through the `drm`
    // crate, which is a different thing and not this module path.
    //
    // The needle is assembled at runtime rather than written as a
    // literal, because this test's own source is one of the files it
    // scans — a literal would match itself and fail. (It did.)
    let mm_path = format!("rustix{}mm{}", "::", "::");
    assert!(
        !include_str!("../src/lib.rs").contains(&mm_path),
        "src/lib.rs reaches for the mm module; the mapping must stay in map.rs"
    );

    // Now the documents. Every place that quotes a count of this crate's
    // `unsafe` blocks must quote the right one.
    //
    // The check is deliberately narrow: take the word **immediately**
    // before "block(s)" on whitespace-normalised text, strip Markdown
    // punctuation, and require that if it is a number word it is the
    // right one. Two earlier versions were wrong in opposite directions,
    // and both were caught only by reintroducing the reviewer's defect
    // and watching what the test did:
    //
    //  * splitting on `\n` and asking whether the fragment mentioned a
    //    number *and* "unsafe" silently **passed** the defect, because
    //    `...and three` and `blocks in nitro-shm...` are different
    //    fragments;
    //  * scanning a 40-character window **failed on clean text**, because
    //    "one `munmap` — four blocks" legitimately contains "one".
    //
    // Only the adjacent word carries the count, so only it is judged.
    let quantifiers = |src: &str| -> Vec<String> {
        let flat = src.split_whitespace().collect::<Vec<_>>().join(" ");
        let words: Vec<&str> = flat.split(' ').collect();
        let mut found = Vec::new();
        for (i, w) in words.iter().enumerate() {
            let bare = w
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            if (bare == "block" || bare == "blocks")
                && let Some(prev) = i.checked_sub(1)
            {
                found.push(
                    words[prev]
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase(),
                );
            }
        }
        found
    };
    for (path, src) in [
        ("DEPENDENCIES.md", include_str!("../../../DEPENDENCIES.md")),
        ("crates/nitro-shm/README.md", include_str!("../README.md")),
        ("crates/nitro-shm/src/map.rs", map),
        ("crates/nitro-shm/src/lib.rs", include_str!("../src/lib.rs")),
    ] {
        for q in quantifiers(src) {
            assert!(
                !(NUMBER_WORDS.contains(&q.as_str()) && q != WORD),
                "{path} quotes \"{q} block(s)\" where this crate's `unsafe` \
                 block count is {WORD}. Every document quoting this number \
                 must agree with `map.rs`. The count has drifted twice \
                 already, and the second time a reviewer had to find it by \
                 hand."
            );
        }
    }
}

/// The `unsafe` exception is scoped by two things, and one of them is a
/// line in `Cargo.toml` that looks like boilerplate and is not.
///
/// `src/map.rs` carries `#![allow(unsafe_code)]`. That is only an
/// *exception* if something is denying `unsafe_code` in the first place,
/// and what does that is `[lints] workspace = true` inheriting the
/// workspace's `unsafe_code = "deny"`. Delete those two lines and the
/// allow silently becomes decoration: `unsafe` would be permitted
/// anywhere in the crate, with no error, no warning, and a module header
/// still claiming the opposite.
///
/// The compiler cannot catch that — the absence of a lint is not a
/// diagnostic — so it is asserted here as a plain text check, which is
/// the only place it can be caught. The sibling half (that `map.rs` is
/// the *only* file allowing it) is enforced by the deny itself: any
/// `unsafe` elsewhere in the crate fails the build, which is what the
/// audit of #569 relied on.
#[test]
fn the_lint_that_scopes_this_exception_is_still_in_place() {
    let manifest = include_str!("../Cargo.toml");
    let has_lints_table = manifest.lines().map(str::trim).any(|l| l == "[lints]");
    let inherits_workspace = manifest
        .lines()
        .map(str::trim)
        .any(|l| l.replace(' ', "") == "workspace=true");
    assert!(
        has_lints_table && inherits_workspace,
        "crates/nitro-shm/Cargo.toml has lost `[lints] workspace = true`. \
         The workspace denies `unsafe_code`; without this table that deny \
         does not apply here, and the `#![allow(unsafe_code)]` in \
         src/map.rs stops being a scoped exception and becomes a licence \
         for the whole crate."
    );

    // And the allow really is scoped to `map.rs`: a crate-level allow in
    // `lib.rs` would widen the exception to everything. Matched at the
    // start of a line so that *prose about* the attribute (the crate docs
    // discuss it, as does the comment explaining why there is no
    // crate-level `forbid`) is not mistaken for the attribute itself —
    // the first version of this check made exactly that mistake and
    // failed on its own documentation.
    let lib = include_str!("../src/lib.rs");
    let lib_allows = lib
        .lines()
        .map(str::trim)
        .any(|l| l.starts_with("#![allow(unsafe_code)") || l.starts_with("#[allow(unsafe_code)"));
    assert!(
        !lib_allows,
        "lib.rs allows unsafe_code; the exception must stay scoped to map.rs"
    );
    let map = include_str!("../src/map.rs");
    assert!(
        map.lines()
            .map(str::trim)
            .any(|l| l == "#![allow(unsafe_code)]"),
        "map.rs has lost its scoped allow"
    );
}
