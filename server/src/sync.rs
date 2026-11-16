//! Locking that carries on after a panic elsewhere. What these mutexes guard is simple (counters,
//! maps, a pending setup), so data a panicking thread left half-updated beats taking the server
//! down with it.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Locks `mutex`, even if a thread panicked while holding it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
