//! Generational keys and the arena they index.
//!
//! A key is a slot index plus the generation the slot had when the key was
//! minted. Removing a value bumps the slot's generation, so every key handed
//! out before the removal is rejected from then on: a stale key is an
//! [`Error::StaleKey`](crate::Error::StaleKey), never a silent hit on a
//! recycled slot.

use std::marker::PhantomData;

/// A generational handle into an [`Arena`].
pub trait Key: Copy + Eq + std::fmt::Debug {
    /// Build a key from its parts (for wire round-trips).
    fn from_parts(index: u32, generation: u32) -> Self;
    /// Slot index.
    fn index(self) -> u32;
    /// Generation stamp.
    fn generation(self) -> u32;
}

/// Define a newtype key implementing [`Key`].
macro_rules! define_key {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name {
            index: u32,
            generation: u32,
        }

        impl $name {
            /// Slot index.
            pub const fn index(self) -> u32 {
                self.index
            }

            /// Generation stamp.
            pub const fn generation(self) -> u32 {
                self.generation
            }

            /// Build a key from its parts.
            ///
            /// Keys are minted by the scene; this exists so a server can put a
            /// key on the wire and get it back. A fabricated key is rejected
            /// like any other stale key.
            pub const fn from_parts(index: u32, generation: u32) -> Self {
                Self { index, generation }
            }
        }

        impl $crate::key::Key for $name {
            fn from_parts(index: u32, generation: u32) -> Self {
                Self { index, generation }
            }
            fn index(self) -> u32 {
                self.index
            }
            fn generation(self) -> u32 {
                self.generation
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}#{}.{}", stringify!($name), self.index, self.generation)
            }
        }
    };
}

pub(crate) use define_key;

#[derive(Debug)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// A generational arena: dense `Vec` storage plus a free list.
#[derive(Debug)]
pub(crate) struct Arena<K, T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    len: usize,
    _key: PhantomData<fn() -> K>,
}

impl<K: Key, T> Arena<K, T> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
            _key: PhantomData,
        }
    }

    pub(crate) fn insert(&mut self, value: T) -> K {
        self.len += 1;
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.value = Some(value);
            return K::from_parts(index, slot.generation);
        }
        let index = u32::try_from(self.slots.len()).expect("arena index space exhausted");
        self.slots.push(Slot {
            generation: 1,
            value: Some(value),
        });
        K::from_parts(index, 1)
    }

    fn slot(&self, key: K) -> Option<&Slot<T>> {
        let slot = self.slots.get(key.index() as usize)?;
        (slot.generation == key.generation()).then_some(slot)
    }

    pub(crate) fn get(&self, key: K) -> Option<&T> {
        self.slot(key)?.value.as_ref()
    }

    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut T> {
        let slot = self.slots.get_mut(key.index() as usize)?;
        if slot.generation != key.generation() {
            return None;
        }
        slot.value.as_mut()
    }

    pub(crate) fn contains(&self, key: K) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn remove(&mut self, key: K) -> Option<T> {
        let slot = self.slots.get_mut(key.index() as usize)?;
        if slot.generation != key.generation() {
            return None;
        }
        let value = slot.value.take()?;
        self.len -= 1;
        // Retire the slot instead of wrapping the generation back onto live
        // keys; wrapping would make an ancient key valid again.
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(key.index());
        }
        Some(value)
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.value.is_some())
            .map(|(i, slot)| K::from_parts(i as u32, slot.generation))
    }
}

#[cfg(test)]
mod tests {
    use super::Arena;

    define_key!(
        /// Test key.
        TestKey
    );

    #[test]
    fn stale_keys_are_rejected() {
        let mut a: Arena<TestKey, u32> = Arena::new();
        let k0 = a.insert(7);
        assert_eq!(a.get(k0), Some(&7));
        assert_eq!(a.remove(k0), Some(7));
        assert_eq!(a.get(k0), None);
        assert!(!a.contains(k0));
        // The slot is recycled with a fresh generation.
        let k1 = a.insert(9);
        assert_eq!(k1.index(), k0.index());
        assert_ne!(k1.generation(), k0.generation());
        assert_eq!(a.get(k0), None);
        assert_eq!(a.get(k1), Some(&9));
        assert_eq!(a.len(), 1);
        assert_eq!(a.keys().collect::<Vec<_>>(), vec![k1]);
    }

    #[test]
    fn out_of_range_keys_are_rejected() {
        let mut a: Arena<TestKey, u32> = Arena::new();
        let bogus = TestKey::from_parts(42, 1);
        assert_eq!(a.get(bogus), None);
        assert_eq!(a.get_mut(bogus), None);
        assert_eq!(a.remove(bogus), None);
        let k = a.insert(1);
        *a.get_mut(k).unwrap() = 2;
        assert_eq!(a.get(k), Some(&2));
    }
}
