//! Texture lifetimes across in-flight frames.
//!
//! A frame that samples a texture holds a reference until its completion
//! fence signals. `Release` of a texture still referenced only *marks* it:
//! the id disappears for new frames at once, but the backend object (and
//! the client buffer behind it) lives until the last referencing frame
//! finishes, and only then does the server hear `Released`.

use std::collections::HashMap;

use crate::backend::BackendError;
use crate::proto::ErrorCode;
use crate::validate::TexInfo;

#[derive(Debug)]
struct Entry<T> {
    tex: T,
    info: TexInfo,
    users: u32,
    released: bool,
}

/// Live textures by id.
#[derive(Debug)]
pub struct TexTable<T> {
    map: HashMap<u32, Entry<T>>,
}

impl<T> Default for TexTable<T> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
        }
    }
}

impl<T> TexTable<T> {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether `id` is taken (by a live or a release-pending texture).
    #[must_use]
    pub fn contains(&self, id: u32) -> bool {
        self.map.contains_key(&id)
    }

    /// Add a texture.
    ///
    /// # Errors
    /// [`ErrorCode::DuplicateId`] if `id` is taken (including by a
    /// texture whose release is still pending); `tex` is handed back.
    pub fn insert(&mut self, id: u32, tex: T, info: TexInfo) -> Result<(), (BackendError, T)> {
        if self.map.contains_key(&id) {
            return Err((
                BackendError::with_code(ErrorCode::DuplicateId, format!("texture {id} exists")),
                tex,
            ));
        }
        self.map.insert(
            id,
            Entry {
                tex,
                info,
                users: 0,
                released: false,
            },
        );
        Ok(())
    }

    /// A live (not released) texture's info.
    #[must_use]
    pub fn info(&self, id: u32) -> Option<TexInfo> {
        self.map.get(&id).filter(|e| !e.released).map(|e| e.info)
    }

    /// A live texture.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<&T> {
        self.map.get(&id).filter(|e| !e.released).map(|e| &e.tex)
    }

    /// A live texture, mutably.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        self.map
            .get_mut(&id)
            .filter(|e| !e.released)
            .map(|e| &mut e.tex)
    }

    /// A frame referencing `ids` was submitted. Each id counts once per
    /// call however often it appears.
    pub fn acquire(&mut self, ids: &[u32]) {
        for id in dedup(ids) {
            if let Some(e) = self.map.get_mut(&id) {
                e.users += 1;
            }
        }
    }

    /// Release `id`. Returns the texture if nothing references it (the
    /// caller frees it now and reports `Released`), `None` if frames are
    /// still in flight (it comes back from [`TexTable::finish`]).
    ///
    /// # Errors
    /// [`ErrorCode::BadId`] for an unknown or already-released id.
    pub fn release(&mut self, id: u32) -> Result<Option<T>, BackendError> {
        match self.map.get_mut(&id) {
            None | Some(Entry { released: true, .. }) => Err(BackendError::with_code(
                ErrorCode::BadId,
                format!("texture {id} unknown"),
            )),
            Some(e) if e.users > 0 => {
                e.released = true;
                Ok(None)
            }
            Some(_) => Ok(self.map.remove(&id).map(|e| e.tex)),
        }
    }

    /// A frame referencing `ids` finished. Returns the released textures
    /// that nothing references any more.
    pub fn finish(&mut self, ids: &[u32]) -> Vec<(u32, T)> {
        let mut out = Vec::new();
        for id in dedup(ids) {
            let done = match self.map.get_mut(&id) {
                Some(e) => {
                    e.users = e.users.saturating_sub(1);
                    e.users == 0 && e.released
                }
                None => false,
            };
            if done && let Some(e) = self.map.remove(&id) {
                out.push((id, e.tex));
            }
        }
        out
    }

    /// Textures held (live and release-pending).
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether no texture is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Remove everything (shutdown).
    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.map.drain().map(|(_, e)| e.tex)
    }
}

fn dedup(ids: &[u32]) -> Vec<u32> {
    let mut v = ids.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::TexKind;

    const INFO: TexInfo = TexInfo {
        kind: TexKind::Shadow,
        w: 1,
        h: 1,
        fourcc: 0,
    };

    #[test]
    fn idle_release_is_immediate() {
        let mut t = TexTable::new();
        t.insert(1, "a", INFO).unwrap();
        assert_eq!(t.release(1).unwrap(), Some("a"));
        assert!(t.is_empty());
        assert_eq!(t.release(1).unwrap_err().code, ErrorCode::BadId);
    }

    #[test]
    fn busy_release_waits_for_every_frame() {
        let mut t = TexTable::new();
        t.insert(1, "a", INFO).unwrap();
        t.acquire(&[1, 1]);
        t.acquire(&[1]);
        assert_eq!(t.release(1).unwrap(), None);
        assert!(t.get(1).is_none(), "gone for new frames");
        assert_eq!(
            t.insert(1, "b", INFO).unwrap_err().0.code,
            ErrorCode::DuplicateId
        );
        assert!(t.finish(&[1, 1]).is_empty());
        assert_eq!(t.finish(&[1]), vec![(1, "a")]);
        assert!(t.is_empty());
    }

    #[test]
    fn finishing_unreleased_keeps_it() {
        let mut t = TexTable::new();
        t.insert(1, "a", INFO).unwrap();
        t.acquire(&[1]);
        assert!(t.finish(&[1]).is_empty());
        assert_eq!(t.get(1), Some(&"a"));
    }
}
