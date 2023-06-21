use std::sync::atomic::{AtomicUsize, Ordering};

pub struct WaitGroup(*mut Inner);

struct Inner(AtomicUsize);

impl WaitGroup {
    pub fn new() -> Self {
        let ptr = Box::into_raw(Box::new(Inner(AtomicUsize::new(1))));
        Self(ptr)
    }

    pub fn waiting_on(&self) -> usize {
        self.count().load(Ordering::Acquire) - 1
    }

    pub fn wait(&self) {
        while self.count().load(Ordering::Acquire) != 1 {
            std::hint::spin_loop();
        }
    }

    pub fn done(self) {}

    fn increment_count(&self) {
        self.count().fetch_add(1, Ordering::Release);
    }

    #[inline(always)]
    fn inner(&self) -> &Inner {
        unsafe { &*self.0 }
    }

    #[inline(always)]
    fn count(&self) -> &AtomicUsize {
        &self.inner().0
    }
}

impl Clone for WaitGroup {
    fn clone(&self) -> Self {
        self.increment_count();
        Self(self.0)
    }
}

impl Drop for WaitGroup {
    fn drop(&mut self) {
        if self.count().fetch_sub(1, Ordering::Relaxed) != 1 {
            return;
        }

        // We're the last WG around so it's safe to drop the `Inner` here.
        // This is also why we don't have to worry about a `clone` incrementing the count here: you
        // need a reference to clone from, but we know we have the last reference inside this call,
        // and it's unique.
        unsafe { drop(Box::from_raw(self.0)) }
    }
}

unsafe impl Sync for WaitGroup {}
unsafe impl Send for WaitGroup {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        thread,
        time::{self, Duration},
    };

    #[test]
    fn simple() {
        let wg = WaitGroup::new();

        let now = time::Instant::now();
        let _: Vec<_> = (0..10)
            .map(|_| {
                let wg = wg.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(1));
                    drop(wg);
                });
            })
            .collect();

        wg.wait();
        assert!(dbg!(now.elapsed().as_millis()) >= 1000);
    }
}
