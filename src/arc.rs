use std::{
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

pub struct Arc<T> {
    inner: NonNull<Inner<T>>,
}

impl<T> Arc<T> {
    pub fn new(value: T) -> Self {
        let ptr = Box::into_raw(Box::new(Inner {
            data: value,
            count: AtomicUsize::new(1),
        }));

        Self {
            inner: unsafe { NonNull::new_unchecked(ptr) },
        }
    }

    #[inline(always)]
    fn inner(&self) -> &Inner<T> {
        unsafe { self.inner.as_ref() }
    }

    #[inline(always)]
    fn count(&self) -> &AtomicUsize {
        &self.inner().count
    }
}

// `Arc` is supposed to make `&T` available across threads so it has to be safe to share `&T`
// across threads.
unsafe impl<T: Sync> Sync for Arc<T> {}

// You can't move `T` out of `Arc` so it's always fine to send it across threads.
unsafe impl<T> Send for Arc<T> {}

impl<T> Deref for Arc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner().data
    }
}

impl<T> Clone for Arc<T> {
    fn clone(&self) -> Self {
        self.count().fetch_add(1, Ordering::Release);
        Self { inner: self.inner }
    }
}

impl<T> Drop for Arc<T> {
    fn drop(&mut self) {
        if self.count().fetch_sub(1, Ordering::Acquire) != 1 {
            return;
        }
        // we're the only `Arc` left, so it's safe to drop `inner`
        unsafe { drop(Box::from_raw(self.inner.as_ptr())) }
    }
}

struct Inner<T> {
    data: T,
    count: AtomicUsize,
}

#[cfg(test)]
mod tests {
    use std::marker::PhantomData;

    use super::*;

    #[test]
    fn test_makes_non_send_type_send() {
        struct Unsend(PhantomData<*mut ()>);

        fn is_send<T: Send>(_: T) {}

        is_send(Arc::new(Unsend(PhantomData)));
    }

    #[test]
    fn test_rwlock_across_threads() {
        let data = String::from("hello");
        let ptr = data.as_bytes() as *const [u8];

        let arc = Arc::new(ptr);

        let arc2 = Arc::clone(&arc);
        let handle = std::thread::spawn(move || unsafe {
            assert_eq!(&**arc2, b"hello");
        });

        unsafe {
            assert_eq!(&**arc, b"hello");
        }

        handle.join().unwrap();
    }
}
