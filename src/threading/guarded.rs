//! A wrapper around a mutex, and a value protected by the mutex.

use core::cell::UnsafeCell;

use crate::Mutex;

/// A wrapper around a mutex, and a value protected by the mutex.
/// This type uses `bun_threading::Mutex` internally.
///
/// Drop-in for `parking_lot::Mutex<T>`: `const fn new(T)`, `.lock()` returns
/// a guard with `Deref`/`DerefMut`, no poisoning.
pub type Guarded<Value> = GuardedBy<Value, Mutex>;

/// A wrapper around a mutex, and a value protected by the mutex.
/// `M` should have `lock` and `unlock` methods.
pub struct GuardedBy<Value, M: RawMutex> {
    /// The raw value. Don't use this if there might be concurrent accesses.
    // `UnsafeCell` is load-bearing: `lock(&self)` hands out `&mut Value` while other `&self`
    // borrows of `GuardedBy` exist (the mutex serializes the actual writers). Without the cell,
    // deriving `&mut Value` from `&self` is UB under Stacked Borrows regardless of the mutex.
    pub(crate) unsynchronized_value: UnsafeCell<Value>,
    mutex: M,
}

// SAFETY: access to `unsynchronized_value` is serialized by `mutex`; `M: RawMutex` provides the
// happens-before edge. `UnsafeCell<Value>` is `!Sync` by default, so re-assert `Sync` here under
// the same bounds a `std::sync::Mutex<Value>` would require.
unsafe impl<Value: Send, M: RawMutex + Sync> Sync for GuardedBy<Value, M> {}

impl<Value, M: RawMutex + Default> GuardedBy<Value, M> {
    /// Creates a guarded value with a default-initialized mutex.
    pub fn init(value: Value) -> Self {
        Self {
            unsynchronized_value: UnsafeCell::new(value),
            mutex: M::default(),
        }
    }
}

impl<Value: Default, M: RawMutex + Default> Default for GuardedBy<Value, M> {
    fn default() -> Self {
        Self::init(Value::default())
    }
}

impl<Value> GuardedBy<Value, Mutex> {
    /// `const` constructor for `static` initializers (`Mutex::new()` is `const`;
    /// `M::default()` in [`init`](Self::init) is not).
    ///
    /// Parity with `parking_lot::Mutex::new` / `parking_lot::const_mutex`.
    pub const fn new(value: Value) -> Self {
        Self {
            unsynchronized_value: UnsafeCell::new(value),
            mutex: Mutex::new(),
        }
    }
}

impl<Value, M: RawMutex> GuardedBy<Value, M> {
    /// Locks the mutex and returns an RAII guard that dereferences to the protected value and
    /// releases the lock on drop.
    pub fn lock(&self) -> GuardedLock<'_, Value, M> {
        self.mutex.lock();
        GuardedLock {
            guarded: self,
            _not_send: core::marker::PhantomData,
        }
    }

    /// Lock-free mutable access when the caller already has `&mut self`
    /// (exclusive borrow proves no other thread can be in the critical
    /// section). Parity with `parking_lot::Mutex::get_mut`.
    #[inline]
    pub fn get_mut(&mut self) -> &mut Value {
        self.unsynchronized_value.get_mut()
    }
}

/// RAII guard returned by [`GuardedBy::lock`]. Dereferences to the protected value and releases
/// the underlying mutex when dropped.
pub struct GuardedLock<'a, Value, M: RawMutex> {
    guarded: &'a GuardedBy<Value, M>,
    // Mutex backends require unlock on the locking thread; shared guards could expose !Sync values.
    _not_send: core::marker::PhantomData<*mut ()>,
}

impl<'a, Value> GuardedLock<'a, Value, Mutex> {
    /// Borrow the raw [`Mutex`] this guard holds. Used by
    /// [`Condition::wait_guarded`](crate::Condition::wait_guarded) to unlock /
    /// re-lock around the OS wait without consuming the guard.
    ///
    /// The returned `&Mutex` has the guard's lifetime, not `'a`, so it cannot
    /// outlive the guard and be used to double-unlock.
    #[inline]
    pub(crate) fn mutex(&self) -> &Mutex {
        &self.guarded.mutex
    }
}

impl<'a, Value, M: RawMutex> core::ops::Deref for GuardedLock<'a, Value, M> {
    type Target = Value;
    #[inline]
    fn deref(&self) -> &Value {
        // SAFETY: the mutex is held for the lifetime of this guard; no other access to
        // `unsynchronized_value` can exist until `Drop` releases it. `UnsafeCell` provides the
        // interior-mutability provenance for this `&self → &Value` projection.
        unsafe { &*self.guarded.unsynchronized_value.get() }
    }
}

impl<'a, Value, M: RawMutex> core::ops::DerefMut for GuardedLock<'a, Value, M> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Value {
        // SAFETY: see `Deref::deref`.
        unsafe { &mut *self.guarded.unsynchronized_value.get() }
    }
}

impl<'a, Value, M: RawMutex> Drop for GuardedLock<'a, Value, M> {
    #[inline]
    fn drop(&mut self) {
        self.guarded.mutex.unlock();
    }
}

/// Trait for the `M` parameter of `GuardedBy`: a raw mutex with `lock`/`unlock`.
pub trait RawMutex {
    fn lock(&self);
    fn unlock(&self);
}

impl RawMutex for Mutex {
    #[inline]
    fn lock(&self) {
        Mutex::lock(self)
    }
    #[inline]
    fn unlock(&self) {
        Mutex::unlock(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{Guarded, GuardedLock, Mutex};
    use crate::Condition;
    use core::cell::Cell;

    macro_rules! assert_not_impl {
        ($ty:ty, $bound:path) => {{
            trait AmbiguousIfImpl<A> {
                fn check() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            struct ImplementsTrait;
            impl<T: ?Sized + $bound> AmbiguousIfImpl<ImplementsTrait> for T {}
            // Inference is ambiguous if the guard implements the forbidden trait.
            let _ = <$ty as AmbiguousIfImpl<_>>::check;
        }};
    }

    #[test]
    fn guard_cannot_move_or_share_across_threads() {
        assert_not_impl!(GuardedLock<'static, u32, Mutex>, Send);
        assert_not_impl!(GuardedLock<'static, u32, Mutex>, Sync);
        assert_not_impl!(GuardedLock<'static, Cell<u32>, Mutex>, Sync);

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Guarded<Cell<u32>>>();
    }

    #[test]
    fn guard_preserves_pointer_sized_layout() {
        assert_eq!(
            core::mem::size_of::<GuardedLock<'_, u32, Mutex>>(),
            core::mem::size_of::<&Guarded<u32>>()
        );
    }

    #[test]
    fn constructors_and_exclusive_access_preserve_values() {
        let mut guarded = Guarded::new(4u32);
        {
            let mut guard = guarded.lock();
            assert_eq!(*guard, 4);
            *guard = 7;
        }
        assert_eq!(*guarded.lock(), 7);
        *guarded.get_mut() = 9;
        assert_eq!(*guarded.lock(), 9);

        let initialized = Guarded::init(11u32);
        assert_eq!(*initialized.lock(), 11);
        let defaulted = Guarded::<u32>::default();
        assert_eq!(*defaulted.lock(), 0);
    }

    #[test]
    fn shared_guarded_value_serializes_contending_writers() {
        let guarded = Guarded::new(Cell::new(0u32));
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    start.wait();
                    for _ in 0..250 {
                        let guard = guarded.lock();
                        guard.set(guard.get() + 1);
                    }
                });
            }
        });
        assert_eq!(guarded.lock().get(), 1_000);
    }

    #[test]
    fn condition_wait_relocks_the_same_guard_on_its_thread() {
        let guarded = Guarded::new(0u32);
        let condition = Condition::new();
        std::thread::scope(|scope| {
            let mut guard = guarded.lock();
            let producer = scope.spawn(|| {
                let mut guard = guarded.lock();
                let initial = *guard;
                *guard = 1;
                condition.notify_one();
                while *guard != 2 {
                    condition.wait_guarded(&mut guard);
                }
                *guard = 3;
                initial
            });

            while *guard == 0 {
                condition.wait_guarded(&mut guard);
            }
            let observed = *guard;
            *guard = 2;
            condition.notify_one();
            drop(guard);
            let initial = producer.join().unwrap();
            assert_eq!(initial, 0);
            assert_eq!(observed, 1);
            assert_eq!(*guarded.lock(), 3);
        });
    }
}
