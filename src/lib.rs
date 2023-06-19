use std::sync::atomic::{AtomicU64, Ordering};

const WRITE_LOCK_FLAG: u64 = 1 << 63;
const WRITE_QUEUE_FLAG: u64 = 1 << 62;
const WRITE_MASK: u64 = WRITE_LOCK_FLAG | WRITE_QUEUE_FLAG;
const READER_MASK: u64 = WRITE_QUEUE_FLAG - 1;

pub struct RWLock {
    lock: AtomicU64,
}

impl RWLock {
    pub const fn new() -> Self {
        Self {
            lock: AtomicU64::new(0),
        }
    }

    pub fn read_lock(&self) {
        // I don't like this but I don't know how else to register a reader without a race
        // try registering a reader; if there is currently a writer, unregister again
        while self.lock.fetch_add(1, Ordering::SeqCst) & WRITE_MASK > 0 {
            self.lock.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn release_read_lock(&self) {
        self.lock.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn write_lock(&self) {
        // try locking
        let is_locked = self.lock.fetch_or(WRITE_LOCK_FLAG, Ordering::SeqCst) & WRITE_LOCK_FLAG > 0;

        // if not already locked, ...
        if !is_locked {
            // ...spin until there are no more readers
            while self.lock.load(Ordering::SeqCst) & READER_MASK > 0 {
                std::hint::spin_loop();
            }
            return;
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
    }

    pub fn release_write_lock(&self) {
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
        static LOCK: RWLock = RWLock::new();

        const N: isize = 100;
        const M: isize = 10000;

        let handles: Vec<_> = (0..N)
            .map(|_| {
                thread::spawn(move || {
                    for _ in 0..M {
                        LOCK.write_lock();
                        unsafe {
                            RESOURCE += 1;
                        }
                        LOCK.release_write_lock();
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        LOCK.read_lock();
        unsafe {
            assert_eq!(RESOURCE, N * M);
        }
        LOCK.release_read_lock();
    }

    #[test]
    fn cannot_write_while_reading() {
        static mut RESOURCE: isize = 10;
        static LOCK: RWLock = RWLock::new();

        let now = std::time::Instant::now();

        let (tx, rx) = std::sync::mpsc::channel();

        let read_threads: Vec<_> = (0..10)
            .map(move |id| {
                let tx = tx.clone();
                thread::spawn(move || {
                    eprintln!("[{id}] getting read lock");
                    LOCK.read_lock();
                    tx.send(()).unwrap();
                    eprintln!("[{id}] got read lock");

                    unsafe {
                        assert_eq!(RESOURCE, 10);
                    }

                    thread::sleep(Duration::from_secs(1));

                    eprintln!("[{id}] releasing read lock");
                    LOCK.release_read_lock();
                    eprintln!("[{id}] released read lock");
                })
            })
            .collect();

        rx.iter().for_each(drop);

        eprintln!("[main] getting write lock");
        LOCK.write_lock();
        eprintln!("[main] got write lock");

        unsafe {
            RESOURCE *= 10;
        }

        eprintln!("[main] releasing write lock");
        LOCK.release_write_lock();
        eprintln!("[main] released write lock");

        assert!(dbg!(now.elapsed()).as_secs() >= 1);

        unsafe {
            assert_eq!(RESOURCE, 100);
        }

        for thread in read_threads {
            thread.join().unwrap();
        }
    }
}
