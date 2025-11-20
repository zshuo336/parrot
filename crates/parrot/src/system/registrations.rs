//! Last-owner storage retires independent registrations outside lock guards.
use crate::owned_payload::PublicationInput;
use std::{collections::HashMap, ops::Deref, sync::RwLock};

pub(crate) trait Entries {
    fn retire(&mut self) -> bool;
}
impl<T> Entries for Option<T> {
    fn retire(&mut self) -> bool {
        PublicationInput::new(self.take()).retire().is_err()
    }
}
impl<T> Entries for Vec<T> {
    fn retire(&mut self) -> bool {
        let mut failed = false;
        for value in self.drain(..) {
            failed |= PublicationInput::new(value).retire().is_err();
        }
        failed
    }
}
impl<K, V> Entries for HashMap<K, V> {
    fn retire(&mut self) -> bool {
        let mut failed = false;
        for (key, value) in self.drain() {
            let key = PublicationInput::new(key);
            let value = PublicationInput::new(value);
            failed |= key.retire().is_err();
            failed |= value.retire().is_err();
        }
        failed
    }
}

pub(crate) struct Registry<T: Entries>(RwLock<T>);
impl<T: Entries> Registry<T> {
    pub(crate) fn new(entries: T) -> Self {
        Self(RwLock::new(entries))
    }
}
impl<T: Entries + Default> Default for Registry<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}
impl<T: Entries> Deref for Registry<T> {
    type Target = RwLock<T>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<T: Entries> Drop for Registry<T> {
    fn drop(&mut self) {
        let failed = self
            .0
            .get_mut()
            .unwrap_or_else(|poison| poison.into_inner())
            .retire();
        // Report a normal Drop failure once. A sibling's failure or an existing
        // caller panic is already unwinding; preserve that original payload.
        if failed && !std::thread::panicking() {
            panic!("registration storage disposal failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        hash::{Hash, Hasher},
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    struct Counted {
        count: Arc<AtomicUsize>,
        throws: bool,
    }
    impl Drop for Counted {
        fn drop(&mut self) {
            self.count.fetch_add(1, Ordering::SeqCst);
            assert!(!self.throws, "entry drop");
        }
    }
    struct Key {
        _owner: Counted,
    }
    impl PartialEq for Key {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }
    impl Eq for Key {}
    impl Hash for Key {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0_u8.hash(state);
        }
    }

    #[test]
    fn poisoned_clean_storage_drops_without_creating_another_failure() {
        let count = Arc::new(AtomicUsize::new(0));
        let storage = Registry::new(vec![
            Counted {
                count: count.clone(),
                throws: false,
            },
            Counted {
                count: count.clone(),
                throws: false,
            },
        ]);
        let original = catch_unwind(AssertUnwindSafe(|| {
            let _guard = storage.write().unwrap();
            panic!("poison fixture");
        }));
        assert!(original.is_err());
        let cleanup = catch_unwind(AssertUnwindSafe(|| drop(storage)));
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert!(cleanup.is_ok());
    }
    #[test]
    fn map_keys_and_values_are_independent_retirement_owners() {
        let count = Arc::new(AtomicUsize::new(0));
        let key = Key {
            _owner: Counted {
                count: count.clone(),
                throws: true,
            },
        };
        let value = Counted {
            count: count.clone(),
            throws: true,
        };
        let storage = Registry::new(HashMap::from([(key, value)]));
        let cleanup = catch_unwind(AssertUnwindSafe(|| drop(storage)));
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let payload = cleanup.expect_err("normal cleanup must report failure");
        assert!(payload.is::<&str>() || payload.is::<String>());
    }
    #[test]
    fn poisoned_throwing_storage_retires_every_entry() {
        let count = Arc::new(AtomicUsize::new(0));
        let storage = Registry::new(vec![
            Counted {
                count: count.clone(),
                throws: true,
            },
            Counted {
                count: count.clone(),
                throws: true,
            },
        ]);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = storage.write().unwrap();
            panic!("poison fixture");
        }));
        let cleanup = catch_unwind(AssertUnwindSafe(|| drop(storage)));
        assert_eq!(count.load(Ordering::SeqCst), 2);
        let payload = cleanup.expect_err("normal cleanup must report failure");
        assert!(payload.is::<&str>() || payload.is::<String>());
    }
}
