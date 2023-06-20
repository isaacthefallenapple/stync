#![feature(strict_provenance)]
#![warn(fuzzy_provenance_casts)]
#![warn(lossy_provenance_casts)]

use std::{
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};

const WRITE_LOCK_FLAG: u64 = 1 << 63;
const WRITE_QUEUE_FLAG: u64 = 1 << 62;
const WRITE_MASK: u64 = WRITE_LOCK_FLAG | WRITE_QUEUE_FLAG;
const READER_MASK: u64 = WRITE_QUEUE_FLAG - 1;

pub struct RWLock<T> {
    lock: AtomicU64,
    data: *mut T,
}

unsafe impl<T: Send + Sync> Sync for RWLock<T> {}

/// It's safe to read from the protected resource until this guard is dropped.
pub struct ReadGuard<'a, T>(&'a RWLock<T>);

impl<'a, T> Deref for ReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: this is safe because we hold the `ReadGuard` and constructing a `RWLock`
        // requires `data` outlive `self`.
        unsafe { &*self.0.data }
    }
}

impl<'a, T> Drop for ReadGuard<'a, T> {
    fn drop(&mut self) {
        self.0.release_read_lock();
    }
}

/// It's safe to write to the protected resource until this guard is dropped.
pub struct WriteGuard<'a, T>(&'a RWLock<T>);

impl<'a, T> Deref for WriteGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: this is safe because no one else can write to `data` while we hold this guard and
        // constructing a `RWLock` requires `data` outlive `self`.
        unsafe { &*self.0.data }
    }
}

impl<'a, T> DerefMut for WriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this is safe because no one else can write to or read from `data` while we hold
        // this guard and constructing a `RWLock` requires `data` outlive `self`.
        unsafe { &mut *self.0.data }
    }
}

impl<'a, T> Drop for WriteGuard<'a, T> {
    fn drop(&mut self) {
        self.0.release_write_lock();
    }
}

impl<T> RWLock<T> {
    /// # Safety
    ///
    ///
    pub const unsafe fn new(data: *mut T) -> Self {
        Self {
            lock: AtomicU64::new(0),
            data,
        }
    }

    pub fn read_lock(&self) -> ReadGuard<'_, T> {
        // I don't like this but I don't know how else to register a reader without a race
        // try registering a reader; if there is currently a writer, unregister again
        while self.lock.fetch_add(1, Ordering::SeqCst) & WRITE_MASK > 0 {
            self.lock.fetch_sub(1, Ordering::SeqCst);
        }

        ReadGuard(self)
    }

    fn release_read_lock(&self) {
        self.lock.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn write_lock(&self) -> WriteGuard<'_, T> {
        let guard = WriteGuard(self);

        // try locking
        let is_locked = self.lock.fetch_or(WRITE_LOCK_FLAG, Ordering::SeqCst) & WRITE_LOCK_FLAG > 0;

        // if not already locked, ...
        if !is_locked {
            // ...spin until there are no more readers
            while self.lock.load(Ordering::SeqCst) & READER_MASK > 0 {
                std::hint::spin_loop();
            }
            return guard;
        }

        // wait until queue is free to queue yourself
        while self.lock.fetch_or(WRITE_QUEUE_FLAG, Ordering::SeqCst) & WRITE_QUEUE_FLAG > 0 {
            std::hint::spin_loop();
        }

        // this writer is now queued; wait until previous lock is released
        while self.lock.fetch_or(WRITE_LOCK_FLAG, Ordering::SeqCst) & WRITE_LOCK_FLAG > 0 {
            std::hint::spin_loop();
        }

        // release queue
        self.lock.fetch_xor(WRITE_QUEUE_FLAG, Ordering::SeqCst);

        guard
    }

    fn release_write_lock(&self) {
        self.lock.fetch_xor(WRITE_LOCK_FLAG, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    #[test]
    fn write_only() {
        static mut RESOURCE: isize = 0;
        static LOCK: RWLock<isize> = unsafe { RWLock::new(&RESOURCE as *const _ as *mut _) };

        const N: isize = 10;
        const M: isize = 10000;

        let handles: Vec<_> = (0..N)
            .map(|_| {
                thread::spawn(move || {
                    for _ in 0..M {
                        *LOCK.write_lock() += 1;
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(*LOCK.read_lock(), N * M);
    }

    #[test]
    fn cannot_write_while_reading() {
        static mut RESOURCE: isize = 10;
        static LOCK: RWLock<isize> = unsafe { RWLock::new(&RESOURCE as *const _ as *mut _) };

        let now = std::time::Instant::now();

        let (tx, rx) = std::sync::mpsc::channel();

        let read_threads: Vec<_> = (0..10)
            .map(move |id| {
                let tx = tx.clone();
                thread::spawn(move || {
                    eprintln!("[{id}] getting read lock");
                    let lock = LOCK.read_lock();
                    tx.send(()).unwrap();
                    eprintln!("[{id}] got read lock");

                    assert_eq!(*lock, 10);

                    thread::sleep(Duration::from_secs(1));

                    eprintln!("[{id}] releasing read lock");
                })
            })
            .collect();

        rx.iter().for_each(drop);

        *LOCK.write_lock() *= 10;

        assert!(dbg!(now.elapsed()).as_secs() >= 1);

        assert_eq!(*LOCK.read_lock(), 100);

        for thread in read_threads {
            thread.join().unwrap();
        }
    }
}
