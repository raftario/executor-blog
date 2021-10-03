use async_task::{Runnable, Task};
use crossbeam_deque::{Injector, Stealer, Worker};
use std::{
    cell::RefCell,
    future::Future,
    hash::{Hash, Hasher},
    iter,
    sync::{
        atomic::{
            AtomicUsize,
            Ordering::{AcqRel, Acquire, Relaxed, Release},
        },
        Arc, RwLock,
    },
    task::{Context, Poll},
    thread,
};
use tracing::{trace, trace_span, Span};

use crate::parking::Parking;

thread_local! {
    static CURRENT: RefCell<Option<WeakExecutor>> = RefCell::new(None);
}

static EXECUTOR_ID: AtomicUsize = AtomicUsize::new(0);

pub struct Executor {
    handle: Arc<Handle>,
}

#[derive(Clone)]
struct WeakExecutor {
    handle: Arc<Handle>,
}

struct Handle {
    asinc: AsyncHandle,
    blocking: BlockingHandle,
    refs: AtomicUsize,
    span: Span,
}

struct AsyncHandle {
    injector: Injector<(Runnable, Span)>,
    stealers: Box<[Stealer<(Runnable, Span)>]>,
    parking: Parking,
    task_id: AtomicUsize,
}

struct BlockingHandle {
    injector: Injector<(Runnable, Span)>,
    stealers: RwLock<Vec<Stealer<(Runnable, Span)>>>,
    parking: Parking,
    task_id: AtomicUsize,
}

pub struct EnterGuard {
    previous: Option<WeakExecutor>,
}

impl Executor {
    pub fn new() -> Self {
        let cpus = num_cpus::get().max(1);
        Self::with_workers(cpus, cpus * 4)
    }

    pub fn with_workers(num_async: usize, num_blocking: usize) -> Self {
        let id = EXECUTOR_ID.fetch_add(1, Relaxed);
        let span = trace_span!(target: "executor", "executor", id = id);

        trace!(
            target: "executor",
            parent: &span,
            "starting executor with {} async and up to {} blocking workers",
            num_async,
            num_blocking,
        );

        let mut stealers = Vec::with_capacity(num_async);
        let mut workers = Vec::with_capacity(num_async);
        for _ in 0..num_async {
            let worker = Worker::new_fifo();
            stealers.push(worker.stealer());
            workers.push(worker);
        }

        let executor = Self {
            handle: Arc::new(Handle {
                asinc: AsyncHandle {
                    injector: Injector::new(),
                    stealers: stealers.into_boxed_slice(),
                    parking: Parking::new(),
                    task_id: AtomicUsize::new(0),
                },
                blocking: BlockingHandle {
                    injector: Injector::new(),
                    stealers: RwLock::new(Vec::with_capacity(num_blocking)),
                    parking: Parking::new(),
                    task_id: AtomicUsize::new(0),
                },
                refs: AtomicUsize::new(1),
                span,
            }),
        };

        for (i, worker) in workers.into_iter().enumerate() {
            trace!(
                target: "executor",
                parent: &executor.handle.span,
                "starting async worker {}",
                i,
            );

            let executor = executor.downgrade();
            thread::Builder::new()
                .name(format!("async-worker-{}", i))
                .spawn(move || async_worker(worker, executor))
                .unwrap();
        }

        executor
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        futures::pin_mut!(future);

        let parking = Arc::new(Parking::new());
        let waker = futures::task::waker(parking.clone());
        let mut context = Context::from_waker(&waker);

        let _guard = self.downgrade().enter();

        let output = loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => break output,
                Poll::Pending => parking.park(),
            }
        };

        output
    }

    pub fn spawn<F: Future>(&self, future: F) -> Task<F::Output>
    where
        F: Send + 'static,
        F::Output: Send,
    {
        let id = self.handle.asinc.task_id.fetch_add(1, Relaxed);
        let span = trace_span!(
            target: "executor",
            parent: &self.handle.span,
            "task",
            id = id,
        );
        let executor = self.downgrade();

        let (runnable, task) = async_task::spawn(future, move |runnable| {
            executor
                .handle
                .asinc
                .injector
                .push((runnable, span.clone()));
            executor.handle.asinc.parking.unpark_one();
        });

        runnable.schedule();
        task
    }

    pub fn spawn_blocking<T, F: FnOnce() -> T>(&self, f: F) -> Task<T>
    where
        F: Send + 'static,
        T: Send + 'static,
    {
        let future = futures::future::lazy(move |_| f());
        let id = self.handle.blocking.task_id.fetch_add(1, Relaxed);
        let span = trace_span!(
            target: "executor",
            parent: &self.handle.span,
            "blocking-task",
            id = id,
        );
        let executor = self.downgrade();

        let (runnable, task) = async_task::spawn(future, move |runnable| {
            let handle = &executor.handle.blocking;

            handle.injector.push((runnable, span.clone()));

            let (should_start_worker, worker_id) = handle
                .parking
                .is_empty()
                .then(|| {
                    handle
                        .stealers
                        .read()
                        .map(|s| (s.len() < s.capacity(), s.len()))
                        .unwrap()
                })
                .unwrap_or((false, 0));

            if should_start_worker {
                trace!(
                    target: "executor",
                    parent: &executor.handle.span,
                    "starting blocking worker {}",
                    worker_id,
                );

                let worker = Worker::new_fifo();
                let stealer = worker.stealer();
                handle.stealers.write().unwrap().push(stealer);

                let executor = executor.clone();
                thread::Builder::new()
                    .name(format!("blocking-worker-{}", worker_id))
                    .spawn(move || blocking_worker(worker, executor))
                    .unwrap();
            } else {
                handle.parking.unpark_one();
            }
        });

        runnable.schedule();
        task
    }

    pub fn current() -> Self {
        CURRENT
            .with(|c| c.borrow().clone())
            .expect("not inside an executor context")
            .upgrade()
            .expect("executor is shutting down")
    }

    pub fn enter(&self) -> EnterGuard {
        self.downgrade().enter()
    }

    fn downgrade(&self) -> WeakExecutor {
        WeakExecutor {
            handle: self.handle.clone(),
        }
    }
}

impl WeakExecutor {
    fn upgrade(self) -> Option<Executor> {
        if self.handle.refs.fetch_add(1, AcqRel) == 0 {
            self.handle.refs.fetch_sub(1, Release);

            trace!(
                target: "executor",
                parent: &self.handle.span,
                "shutting down executor",
            );
            self.handle.asinc.parking.unpark_all();
            self.handle.blocking.parking.unpark_all();

            return None;
        }

        Some(Executor {
            handle: self.handle,
        })
    }

    fn enter(self) -> EnterGuard {
        let previous = CURRENT.with(|c| c.borrow_mut().replace(self));
        EnterGuard { previous }
    }
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    CURRENT
        .with(|c| c.borrow().clone())
        .map(|e| e.upgrade().expect("executor is shutting down"))
        .unwrap_or_default()
        .block_on(future)
}

pub fn spawn<F: Future>(future: F) -> Task<F::Output>
where
    F: Send + 'static,
    F::Output: Send,
{
    Executor::current().spawn(future)
}

pub fn spawn_blocking<T, F: FnOnce() -> T>(f: F) -> Task<T>
where
    F: Send + 'static,
    T: Send + 'static,
{
    Executor::current().spawn_blocking(f)
}

fn async_worker(worker: Worker<(Runnable, Span)>, executor: WeakExecutor) {
    let _guard = executor.clone().enter();

    let handle = &executor.handle.asinc;
    let refs = &executor.handle.refs;

    loop {
        let task = worker.pop().or_else(|| {
            iter::repeat_with(|| {
                handle
                    .injector
                    .steal_batch_and_pop(&worker)
                    .or_else(|| handle.stealers.iter().map(|s| s.steal()).collect())
            })
            .find(|s| !s.is_retry())
            .and_then(|s| s.success())
        });

        match task {
            Some((task, span)) => {
                let _span = span.enter();
                task.run();
            }
            None if refs.load(Acquire) == 0 => break,
            None => handle.parking.park(),
        }
    }

    trace!(
        target: "executor",
        parent: &executor.handle.span,
        "shutting down worker",
    );
}

fn blocking_worker(worker: Worker<(Runnable, Span)>, executor: WeakExecutor) {
    let _guard = executor.clone().enter();

    let handle = &executor.handle.blocking;
    let refs = &executor.handle.refs;

    loop {
        let task = worker.pop().or_else(|| {
            iter::repeat_with(|| {
                handle.injector.steal_batch_and_pop(&worker).or_else(|| {
                    handle
                        .stealers
                        .read()
                        .unwrap()
                        .iter()
                        .map(|s| s.steal())
                        .collect()
                })
            })
            .find(|s| !s.is_retry())
            .and_then(|s| s.success())
        });

        match task {
            Some((task, span)) => {
                let _span = span.enter();
                task.run();
            }
            None if refs.load(Acquire) == 0 => break,
            None => handle.parking.park(),
        }
    }

    trace!(
        target: "executor",
        parent: &executor.handle.span,
        "shutting down worker",
    );
}

impl Clone for Executor {
    fn clone(&self) -> Self {
        self.handle.refs.fetch_add(1, Release);
        Self {
            handle: self.handle.clone(),
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        if self.handle.refs.fetch_sub(1, AcqRel) == 1 {
            trace!(
                target: "executor",
                parent: &self.handle.span,
                "shutting down executor",
            );
            self.handle.asinc.parking.unpark_all();
            self.handle.blocking.parking.unpark_all();
        }
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for Executor {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.handle, &other.handle)
    }
}
impl Eq for Executor {}

impl Hash for Executor {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.handle).hash(state);
    }
}

impl Drop for EnterGuard {
    fn drop(&mut self) {
        CURRENT.with(|c| *c.borrow_mut() = self.previous.take());
    }
}
