#![allow(clippy::mutex_atomic)]

use futures::task::ArcWake;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Condvar, Mutex,
};

pub struct Parking {
    skip: AtomicBool,
    waiting: Mutex<usize>,
    signal: Condvar,
}

impl Parking {
    pub fn new() -> Self {
        Self {
            skip: AtomicBool::new(false),
            waiting: Mutex::new(0),
            signal: Condvar::new(),
        }
    }

    pub fn park(&self) {
        let mut waiting = self.waiting.lock().unwrap();

        if !self.skip.swap(false, Ordering::SeqCst) {
            *waiting += 1;
            let mut waiting = self.signal.wait(waiting).unwrap();
            *waiting -= 1;
        }
    }

    pub fn unpark_one(&self) {
        if !self.skip.load(Ordering::SeqCst) {
            let waiting = self.waiting.lock().unwrap();

            if *waiting > 0 {
                self.signal.notify_one();
            } else {
                self.skip.store(true, Ordering::SeqCst);
            }
        }
    }

    pub fn unpark_all(&self) {
        let _waiting = self.waiting.lock();
        self.signal.notify_all();
    }
}

impl ArcWake for Parking {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.unpark_one();
    }
}
