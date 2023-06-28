use std::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicU64, Ordering},
};

/// If this bit is set, the a writer is currently holding the lock.
const WRITE_LOCK_FLAG: u64 = 1 << 63;
/// If this bit is set, a writer wants to acquire the lock.
const WRITE_QUEUE_FLAG: u64 = 1 << 62;
/// If any of these bits are set, no readers should be acquiring the lock.
const WRITE_MASK: u64 = WRITE_LOCK_FLAG | WRITE_QUEUE_FLAG;
const READER_MASK: u64 = WRITE_QUEUE_FLAG - 1;

pub struct RWLock<T> {
    lock: AtomicU64,
    data: UnsafeCell<T>,
}

// `T` has to be `Send` because `WriteLock` is `Send`.
unsafe impl<T: Send + Sync> Sync for RWLock<T> {}

/// It's safe to read from the protected resource until this guard is dropped.
pub struct ReadGuard<'a, T>(&'a RWLock<T>);

impl<'a, T> Deref for ReadGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: this is safe because we hold the `ReadGuard` and constructing a `RWLock`
        // requires `data` outlive `self`.
        unsafe { &*self.0.data.get() }
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
        unsafe { &*self.0.data.get() }
    }
}

impl<'a, T> DerefMut for WriteGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: this is safe because no one else can write to or read from `data` while we hold
        // this guard and constructing a `RWLock` requires `data` outlive `self`.
        unsafe { &mut *self.0.data.get() }
    }
}

impl<'a, T> Drop for WriteGuard<'a, T> {
    fn drop(&mut self) {
        self.0.release_write_lock();
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TryReadError {
    HasWriter,
    QueuedWriter,
}

#[derive(Debug, Eq, PartialEq)]
pub enum TryWriteError {
    HasWriter,
    QueuedWriter,
    HasReaders(u64),
}

impl<T> RWLock<T> {
    pub const fn new(value: T) -> Self {
        Self {
            lock: AtomicU64::new(0),
            data: UnsafeCell::new(value),
        }
    }

    /// Blocks the thread until it's safe to read the data.
    pub fn read(&self) -> ReadGuard<'_, T> {
        // I don't like this but I don't know how else to register a reader without a race
        // try registering a reader; if there is currently a writer, unregister again
        while self.lock.fetch_add(1, Ordering::SeqCst) & WRITE_MASK > 0 {
            self.lock.fetch_sub(1, Ordering::SeqCst);
        }

        ReadGuard(self)
    }

    /// Try to acquire read access. Will not block the thread.
    pub fn try_read(&self) -> Result<ReadGuard<'_, T>, TryReadError> {
        let current = self.lock.fetch_add(1, Ordering::SeqCst);
        if current & WRITE_MASK > 0 {
            self.lock.fetch_sub(1, Ordering::SeqCst);

            return Err(if current & WRITE_LOCK_FLAG > 0 {
                TryReadError::HasWriter
            } else {
                TryReadError::QueuedWriter
            });
        }

        Ok(ReadGuard(self))
    }

    fn release_read_lock(&self) {
        self.lock.fetch_sub(1, Ordering::SeqCst);
    }

    /// Blocks the thread until it's safe to write the data.
    pub fn write(&self) -> WriteGuard<'_, T> {
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

    /// Try to acquire write access. Will not block the thread.
    pub fn try_write(&self) -> Result<WriteGuard<'_, T>, TryWriteError> {
        // try locking
        let current = self.lock.fetch_or(WRITE_LOCK_FLAG, Ordering::SeqCst);
        let is_locked = current & WRITE_MASK > 0;

        if is_locked {
            return Err(if current & WRITE_LOCK_FLAG > 0 {
                TryWriteError::HasWriter
            } else {
                TryWriteError::QueuedWriter
            });
        }

        let readers = self.lock.load(Ordering::SeqCst) & READER_MASK;
        if readers > 0 {
            self.lock.fetch_xor(WRITE_LOCK_FLAG, Ordering::SeqCst);
            return Err(TryWriteError::HasReaders(readers));
        }

        Ok(WriteGuard(self))
    }

    fn release_write_lock(&self) {
        self.lock.fetch_xor(WRITE_LOCK_FLAG, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wait_group;
    use std::{thread, time::Duration};

    #[test]
    fn write_only() {
        static LOCK: RWLock<isize> = RWLock::new(0);

        const N: isize = 10;
        const M: isize = 10000;

        let handles: Vec<_> = (0..N)
            .map(|_| {
                thread::spawn(move || {
                    for _ in 0..M {
                        *LOCK.write() += 1;
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(*LOCK.read(), N * M);
    }

    #[test]
    #[cfg_attr(not(feature = "timed"), ignore)]
    fn cannot_write_while_reading() {
        use wait_group::manual::WaitGroup;

        static LOCK: RWLock<isize> = RWLock::new(10);

        let now = std::time::Instant::now();

        const TASKS: usize = if cfg!(miri) { 3 } else { 100 };
        static WG: WaitGroup = WaitGroup::new(TASKS);

        let read_threads: Vec<_> = (0..TASKS)
            .map(|id| {
                thread::spawn(move || {
                    eprintln!("[{id}] getting read lock");
                    let lock = LOCK.read();
                    WG.done();
                    eprintln!("[{id}] got read lock");

                    assert_eq!(*lock, 10);

                    thread::sleep(Duration::from_secs(1));

                    eprintln!("[{id}] releasing read lock");
                })
            })
            .collect();

        WG.wait();

        *LOCK.write() *= 10;

        assert!(dbg!(now.elapsed()).as_secs() >= 1);

        assert_eq!(*LOCK.read(), 100);

        for thread in read_threads {
            thread.join().unwrap();
        }
    }

    #[test]
    fn test_try() {
        static LOCK: RWLock<isize> = RWLock::new(0);

        for _ in 0..10 {
            let read = LOCK.read();

            assert!(LOCK.try_write().is_err());

            drop(read);

            let write = LOCK.try_write().expect("couldn't acquire lock");

            assert!(LOCK.try_read().is_err());

            drop(write);

            // reads don't block each other
            let _read = LOCK.try_read().expect("couldn't acquire lock");
            let _read = LOCK.read();
            let _ = LOCK.try_read().expect("couldn't acquire lock");
        }
    }

    #[test]
    #[cfg_attr(not(feature = "timed"), ignore)]
    fn test_try_write_doesnt_block() {
        use wait_group::raii::WaitGroup;

        static LOCK: RWLock<isize> = RWLock::new(0);

        let now = std::time::Instant::now();

        let wg = WaitGroup::new();

        let wg_write = wg.clone();
        let write_handle = thread::spawn(move || {
            let write = LOCK.write();
            wg_write.done();
            thread::sleep(Duration::from_secs(1));
            drop(write);
        });

        let try_write_handle = thread::spawn(move || {
            wg.wait();
            assert!(LOCK.try_write().is_err());
        });

        for handle in [write_handle, try_write_handle] {
            handle.join().unwrap();
        }

        assert!(now.elapsed().as_millis() <= 1100);
    }

    #[test]
    fn test_try_read_fails_with_reader_count() {
        static LOCK: RWLock<isize> = RWLock::new(0);

        const N: u64 = 100;
        let _readers: Vec<_> = (0..N).map(|_| LOCK.read()).collect();

        match LOCK.try_write() {
            Err(TryWriteError::HasReaders(n)) => {
                assert_eq!(n, N);
            }
            Err(e) => {
                panic!("wrong error: {e:?}");
            }
            Ok(_) => {
                panic!("got write lock while readers there are readers");
            }
        }
    }
}
