//! [`Live`]: a value the running server reads through, so a configuration change can replace it.
//!
//! A hot setting's reader (`crate::reload`) holds a `Live<T>` instead of a `T`. Every clone
//! shares one cell: the server's configuration applier ([`Live::set`]) and every request handler
//! that reads it ([`Live::get`]) see the same value, and a handler sees the new one on its next
//! read. A read is a lock held for one `Arc` clone, cheap enough for every request.

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

/// A shared, replaceable value. See the module docs.
pub struct Live<T> {
    cell: Arc<RwLock<Arc<T>>>,
}

impl<T> Live<T> {
    /// A cell holding `value`.
    #[must_use]
    pub fn new(value: T) -> Self {
        Self::from_arc(Arc::new(value))
    }

    /// A cell holding `value`, already shared.
    #[must_use]
    pub fn from_arc(value: Arc<T>) -> Self {
        Self {
            cell: Arc::new(RwLock::new(value)),
        }
    }

    /// The value in force now.
    #[must_use]
    pub fn get(&self) -> Arc<T> {
        self.cell
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Replaces the value for every clone. Readers holding the old one keep it until they read
    /// again.
    pub fn set(&self, value: T) {
        *self.cell.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(value);
    }
}

impl<T> Clone for Live<T> {
    fn clone(&self) -> Self {
        Self {
            cell: self.cell.clone(),
        }
    }
}

impl<T: Default> Default for Live<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for Live<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Live").field(&*self.get()).finish()
    }
}

impl<T> From<T> for Live<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_clone_sees_a_set() {
        let a = Live::new(1);
        let b = a.clone();
        let held = a.get();
        b.set(2);
        assert_eq!(*a.get(), 2);
        assert_eq!(*held, 1, "a reader keeps what it read");
    }
}
