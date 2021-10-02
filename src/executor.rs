use async_task::{Runnable, Task};
use crossbeam_deque::{Injector, Stealer, Worker};
use futures::task::AtomicWaker;
use std::{
    cell::RefCell,
    future::Future,
    iter,
    pin::Pin,
    sync::{
        atomic::{
            AtomicBool, AtomicUsize,
            Ordering::{Acquire, Release, SeqCst},
        },
        Arc,
    },
    task::{Context, Poll},
    thread::{self, JoinHandle},
};

use crate::parking::Parking;

thread_local! {
    static CURRENT: RefCell<Option<WeakExecutor>> = RefCell::new(None);
}

pub struct Executor {
    handle: Arc<Handle>,
}

#[derive(Clone)]
struct WeakExecutor {
    handle: Arc<Handle>,
}

struct Handle {
    injector: Injector<Runnable>,
    stealers: Box<[Stealer<Runnable>]>,
    parking: Parking,
    refs: AtomicUsize,
}

pub struct BlockingTask<T> {
    handle: Option<JoinHandle<T>>,
    shared: Arc<(AtomicWaker, AtomicBool)>,
}

impl Executor {
    pub fn new() -> Self {
        Self::with_workers(num_cpus::get().max(1))
    }

    pub fn with_workers(num_workers: usize) -> Self {
        let mut workers = Vec::with_capacity(num_workers);
        let mut stealers = Vec::with_capacity(num_workers);
        for _ in 0..num_workers {
            let worker = Worker::new_fifo();
            stealers.push(worker.stealer());
            workers.push(worker);
        }

        let executor = Self {
            handle: Arc::new(Handle {
                injector: Injector::new(),
                stealers: stealers.into_boxed_slice(),
                parking: Parking::new(),
                refs: AtomicUsize::new(1),
            }),
        };

        for worker in workers {
            let executor = executor.downgrade();
            thread::spawn(move || worker_loop(worker, executor));
        }

        executor
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        futures::pin_mut!(future);

        let parking = Arc::new(Parking::new());
        let waker = futures::task::waker(parking.clone());
        let mut context = Context::from_waker(&waker);

        set_current(self.downgrade());

        let output = loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => break output,
                Poll::Pending => parking.park(),
            }
        };

        clear_current();
        output
    }

    pub fn spawn<F: Future>(&self, future: F) -> Task<F::Output>
    where
        F: Send + 'static,
        F::Output: Send,
    {
        let handle = self.handle.clone();

        let (runnable, task) = async_task::spawn(future, move |runnable| {
            handle.injector.push(runnable);
            handle.parking.unpark_one();
        });

        runnable.schedule();
        task
    }

    pub fn spawn_blocking<T, F: FnOnce() -> T>(&self, f: F) -> BlockingTask<T>
    where
        F: Send + 'static,
        T: Send + 'static,
    {
        let shared = Arc::new((AtomicWaker::new(), AtomicBool::new(false)));

        let thread_shared = shared.clone();
        let thread_executor = self.downgrade();

        let handle = thread::spawn(move || {
            set_current(thread_executor);

            let output = f();
            thread_shared.1.store(true, Release);
            thread_shared.0.wake();

            clear_current();

            output
        });

        BlockingTask {
            handle: Some(handle),
            shared,
        }
    }

    fn downgrade(&self) -> WeakExecutor {
        WeakExecutor {
            handle: self.handle.clone(),
        }
    }
}

impl WeakExecutor {
    fn upgrade(&self) -> Option<Executor> {
        if self.handle.refs.fetch_add(1, SeqCst) == 0 {
            self.handle.refs.fetch_sub(1, SeqCst);
            self.handle.parking.unpark_all();
            return None;
        }

        Some(Executor {
            handle: self.handle.clone(),
        })
    }
}

pub fn spawn<F: Future>(future: F) -> Task<F::Output>
where
    F: Send + 'static,
    F::Output: Send,
{
    current()
        .expect("cannot implicitely spawn tasks outside of an executor context")
        .upgrade()
        .expect("executor is shut down")
        .spawn(future)
}

pub fn spawn_blocking<T, F: FnOnce() -> T>(f: F) -> BlockingTask<T>
where
    F: Send + 'static,
    T: Send + 'static,
{
    current()
        .expect("cannot implicitely spawn tasks outside of an executor context")
        .upgrade()
        .expect("executor is shut down")
        .spawn_blocking(f)
}

fn current() -> Option<WeakExecutor> {
    CURRENT.with(|c| c.borrow().clone())
}

fn set_current(executor: WeakExecutor) {
    CURRENT.with(move |c| *c.borrow_mut() = Some(executor));
}

fn clear_current() {
    CURRENT.with(move |c| *c.borrow_mut() = None);
}

fn worker_loop(worker: Worker<Runnable>, executor: WeakExecutor) {
    set_current(executor.clone());

    loop {
        let task = worker.pop().or_else(|| {
            iter::repeat_with(|| {
                executor
                    .handle
                    .injector
                    .steal_batch_and_pop(&worker)
                    .or_else(|| executor.handle.stealers.iter().map(|s| s.steal()).collect())
            })
            .find(|s| !s.is_retry())
            .and_then(|s| s.success())
        });

        match task {
            Some(task) => {
                task.run();
            }
            None if executor.handle.refs.load(SeqCst) == 0 => break,
            None => executor.handle.parking.park(),
        }
    }

    clear_current();
}

impl Clone for Executor {
    fn clone(&self) -> Self {
        self.handle.refs.fetch_add(1, SeqCst);
        Self {
            handle: self.handle.clone(),
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        if self.handle.refs.fetch_sub(1, SeqCst) == 1 {
            self.handle.parking.unpark_all();
        }
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Future for BlockingTask<T> {
    type Output = thread::Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.shared.1.load(Acquire) {
            Poll::Ready(self.handle.take().unwrap().join())
        } else {
            self.shared.0.register(cx.waker());

            if self.shared.1.load(Acquire) {
                Poll::Ready(self.handle.take().unwrap().join())
            } else {
                Poll::Pending
            }
        }
    }
}
