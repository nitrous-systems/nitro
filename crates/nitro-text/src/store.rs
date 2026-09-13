//! Storage for shaped text, keyed and owned.
//!
//! The server shapes a client's text once and keeps the result here; scene
//! nodes refer to a [`TextKey`]. Every entry records the client that owns it,
//! so a disconnect drops the lot in one call.

use std::collections::HashMap;

use crate::layout::ShapedText;

/// Handle to a [`ShapedText`] in a [`TextStore`].
///
/// Keys are handed out from a counter that only ever counts up, so a key that
/// outlives its entry misses rather than aliasing a later one. The counter
/// wraps at `u32::MAX` — after four billion `SetText`s in one session it
/// could in principle collide with a key still in use, which needs a client
/// re-setting text every frame for a couple of years. Worth knowing about,
/// not worth a wider key: the failure mode is a label drawing the wrong
/// string, not memory unsafety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextKey(pub u32);

/// One entry: the text and its owner.
#[derive(Debug)]
struct Entry {
    owner: u32,
    text: ShapedText,
}

/// The shaped-text store.
#[derive(Debug, Default)]
pub struct TextStore {
    entries: HashMap<TextKey, Entry>,
    /// Keys per owner, so `remove_owner` does not scan the whole map.
    by_owner: HashMap<u32, Vec<TextKey>>,
    next: u32,
}

impl TextStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert shaped text owned by `owner` (the server's client id).
    pub fn insert(&mut self, owner: u32, text: ShapedText) -> TextKey {
        let key = TextKey(self.next);
        self.next = self.next.wrapping_add(1);
        self.entries.insert(key, Entry { owner, text });
        self.by_owner.entry(owner).or_default().push(key);
        key
    }

    /// Look up shaped text.
    #[must_use]
    pub fn get(&self, key: TextKey) -> Option<&ShapedText> {
        self.entries.get(&key).map(|e| &e.text)
    }

    /// Remove one entry, returning it.
    pub fn remove(&mut self, key: TextKey) -> Option<ShapedText> {
        let entry = self.entries.remove(&key)?;
        if let Some(keys) = self.by_owner.get_mut(&entry.owner) {
            keys.retain(|k| *k != key);
            if keys.is_empty() {
                self.by_owner.remove(&entry.owner);
            }
        }
        Some(entry.text)
    }

    /// Drop everything `owner` owns — what a client disconnect calls.
    pub fn remove_owner(&mut self, owner: u32) {
        if let Some(keys) = self.by_owner.remove(&owner) {
            for key in keys {
                self.entries.remove(&key);
            }
        }
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the store holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(width: f32) -> ShapedText {
        ShapedText {
            width,
            ..ShapedText::default()
        }
    }

    #[test]
    fn insert_get_remove_round_trip() {
        let mut store = TextStore::new();
        assert!(store.is_empty());
        let a = store.insert(1, text(10.0));
        let b = store.insert(2, text(20.0));
        assert_eq!(store.len(), 2);
        assert!((store.get(a).unwrap().width - 10.0).abs() < f32::EPSILON);
        assert!((store.remove(b).unwrap().width - 20.0).abs() < f32::EPSILON);
        assert!(store.get(b).is_none());
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn remove_owner_drops_only_that_owners_text() {
        let mut store = TextStore::new();
        let a = store.insert(1, text(1.0));
        let b = store.insert(1, text(2.0));
        let c = store.insert(7, text(3.0));
        store.remove_owner(1);
        assert!(store.get(a).is_none());
        assert!(store.get(b).is_none());
        assert!(store.get(c).is_some());
        assert_eq!(store.len(), 1);
        // Removing an owner twice, or an unknown one, is a no-op.
        store.remove_owner(1);
        store.remove_owner(99);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn keys_are_not_reused() {
        let mut store = TextStore::new();
        let a = store.insert(1, text(1.0));
        store.remove(a);
        let b = store.insert(1, text(1.0));
        assert_ne!(a, b);
    }
}
