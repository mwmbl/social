//! How this crate reaches the SQLite store.

use std::sync::{Arc, Mutex, PoisonError};

use store::Store;

/// Gives scoped access to the [`Store`], which is not `Sync`.
///
/// The closure is synchronous, so a lock can never be held across an `.await`. The binary
/// usually implements this over a `Mutex<Store>`, or a dedicated database thread.
pub trait StoreAccess: Send + Sync {
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> R) -> R;
}

impl StoreAccess for Mutex<Store> {
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        // A panic while holding the lock cannot leave SQLite in a state we would mishandle,
        // so a poisoned lock is simply taken over.
        let guard = self.lock().unwrap_or_else(PoisonError::into_inner);
        f(&guard)
    }
}

impl<T: StoreAccess + ?Sized> StoreAccess for Arc<T> {
    fn with_store<R>(&self, f: impl FnOnce(&Store) -> R) -> R {
        (**self).with_store(f)
    }
}
