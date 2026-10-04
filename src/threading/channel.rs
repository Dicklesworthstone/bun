// This file contains code derived from the following source:
//   https://gist.github.com/kprotty/0d2dc3da4840341d6ff361b27bdac7dc#file-sync2-zig

use core::cell::{Cell, UnsafeCell};
use core::mem::MaybeUninit;

use bun_collections::LinearFifo;
use bun_collections::linear_fifo::{DynamicBuffer, LinearFifoBuffer, StaticBuffer};

use crate::Condition;
use crate::Mutex;

#[derive(thiserror::Error, strum::IntoStaticStr, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelError {
    #[error("Closed")]
    Closed,
    #[error("OutOfMemory")]
    OutOfMemory,
}

bun_core::oom_from_alloc!(ChannelError);

// Channel is shared across threads through `&self`, so `buffer` is wrapped in
// `UnsafeCell` and `is_closed` in `Cell`, both accessed only while `mutex` is
// held. The buffer strategy is a `LinearFifoBuffer<T>` trait param
// (`Channel<T, B: LinearFifoBuffer<T>>`) with per-buffer inherent constructors
// below.
pub struct Channel<T, B: LinearFifoBuffer<T> = DynamicBuffer<T>> {
    mutex: Mutex,
    putters: Condition,
    getters: Condition,
    buffer: UnsafeCell<LinearFifo<T, B>>,
    // `Cell` (not `UnsafeCell`): `bool` is `Copy`, so safe `.get()/.set()` are
    // exactly the non-atomic load/store the mutex already serializes. The
    // `unsafe impl Sync` below is where the cross-thread safety burden lives.
    is_closed: Cell<bool>,
}

// SAFETY: all interior-mutable state is guarded by `mutex`.
unsafe impl<T: Send, B: LinearFifoBuffer<T>> Send for Channel<T, B> {}
// SAFETY: all interior-mutable state is guarded by `mutex`.
unsafe impl<T: Send, B: LinearFifoBuffer<T>> Sync for Channel<T, B> {}

// Rust cannot dispatch a single `init` ident to different signatures based on
// a type-level discriminant. Callers pick the matching constructor directly.

impl<T: Copy, const N: usize> Channel<T, StaticBuffer<T, N>> {
    #[inline]
    pub fn init_static() -> Self {
        Self::with_buffer(LinearFifo::<T, StaticBuffer<T, N>>::init())
    }
}

// `T: Copy` because `LinearFifo::write`/`read` are slice-copy based. All
// in-tree channel payloads are POD; revisit if a non-`Copy` T appears.
impl<T: Copy, B: LinearFifoBuffer<T>> Channel<T, B> {
    fn with_buffer(buffer: LinearFifo<T, B>) -> Self {
        Self {
            mutex: Mutex::default(),
            putters: Condition::default(),
            getters: Condition::default(),
            buffer: UnsafeCell::new(buffer),
            is_closed: Cell::new(false),
        }
    }

    pub fn write_item(&self, item: T) -> Result<(), ChannelError> {
        self.write_all(core::slice::from_ref(&item))
    }

    pub fn read_item(&self) -> Result<T, ChannelError> {
        let mut items = [MaybeUninit::uninit()];
        self.read_all(&mut items)?;
        // SAFETY: successful read_all initialized the entire output slice.
        Ok(unsafe { items[0].assume_init_read() })
    }

    pub(crate) fn write_all(&self, items: &[T]) -> Result<(), ChannelError> {
        let n = self.write_items(items, true)?;
        debug_assert!(n == items.len());
        Ok(())
    }

    pub(crate) fn read_all(&self, items: &mut [MaybeUninit<T>]) -> Result<(), ChannelError> {
        let n = self.read_items(items, true)?;
        debug_assert!(n == items.len());
        Ok(())
    }

    fn write_items(&self, items: &[T], should_block: bool) -> Result<usize, ChannelError> {
        let _guard = self.mutex.lock_guard();

        let mut pushed: usize = 0;
        while pushed < items.len() {
            // Re-derive the `&mut buffer` each iteration: `Condition::wait`
            // below releases the mutex, so a long-lived `&mut buffer` held
            // across wait() would alias another thread's `&mut` (UB).
            // `is_closed` is a `Cell` so `.get()` is already a fresh load each
            // iteration (cannot be hoisted past the interior-mutable wait).
            let did_push = 'blk: {
                if self.is_closed.get() {
                    return Err(ChannelError::Closed);
                }
                // SAFETY: mutex is held; this &mut does not live across wait().
                let buffer = unsafe { &mut *self.buffer.get() };
                match buffer.write_item(items[pushed]) {
                    Ok(()) => {}
                    Err(err) => {
                        if B::DYNAMIC {
                            return Err(err.into());
                        }
                        break 'blk false;
                    }
                }
                self.getters.signal();
                break 'blk true;
            };

            if did_push {
                pushed += 1;
            } else if should_block {
                // wait() releases the mutex while parked, reacquires before
                // returning. No long-lived UnsafeCell borrows are live here.
                self.putters.wait(&self.mutex);
            } else {
                break;
            }
        }

        Ok(pushed)
    }

    fn read_items(
        &self,
        items: &mut [MaybeUninit<T>],
        should_block: bool,
    ) -> Result<usize, ChannelError> {
        let _guard = self.mutex.lock_guard();

        let mut popped: usize = 0;
        while popped < items.len() {
            if let Some(item) = self.read_item_locked()? {
                items[popped].write(item);
                popped += 1;
            } else if should_block {
                self.getters.wait(&self.mutex);
            } else {
                break;
            }
        }

        Ok(popped)
    }

    fn read_item_locked(&self) -> Result<Option<T>, ChannelError> {
        // SAFETY: the caller holds the mutex; this borrow ends before wait().
        let buffer = unsafe { &mut *self.buffer.get() };
        if buffer.readable_length() == 0 {
            return if self.is_closed.get() {
                Err(ChannelError::Closed)
            } else {
                Ok(None)
            };
        }
        let item = buffer
            .read_item()
            .expect("readable_length checked before read_item");
        self.putters.signal();
        Ok(Some(item))
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, ChannelError};

    #[test]
    fn single_item_reads_support_non_byte_payloads() {
        let channel = Channel::<bool, super::StaticBuffer<bool, 2>>::init_static();
        {
            let _guard = channel.mutex.lock_guard();
            assert_eq!(channel.read_item_locked(), Ok(None));
        }
        channel.write_item(true).unwrap();
        assert_eq!(channel.read_item(), Ok(true));
        channel.write_item(false).unwrap();
        assert_eq!(channel.read_item(), Ok(false));
        {
            let _guard = channel.mutex.lock_guard();
            channel.is_closed.set(true);
        }
        assert_eq!(channel.read_item(), Err(ChannelError::Closed));
    }

    #[test]
    fn multi_item_write_appends_each_item_once() {
        let channel = Channel::<u8, super::StaticBuffer<u8, 8>>::init_static();
        assert_eq!(channel.write_items(&[1, 2, 3], false), Ok(3));
        let mut out = [core::mem::MaybeUninit::uninit(); 3];
        channel.read_all(&mut out).unwrap();
        // SAFETY: successful read_all initialized all three slots.
        assert_eq!(out.map(|item| unsafe { item.assume_init() }), [1, 2, 3]);
        assert_eq!(channel.read_items(&mut out, false), Ok(0));
    }
}
