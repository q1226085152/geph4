//! A global, auto-scaling, preemptive scheduler based on `async-executor`.
//!
//! `smolscale` is a fairly thin wrapper around a global [`async-executor`]. Unlike `async-global-executor` and friends, however, it has a **preemptive** thread pool that ensures that tasks cannot block other tasks no matter what. This means that you can do things like run expensive computations or even do blocking I/O within a task without worrying about causing deadlocks. Even with "traditional" tasks that do not block, this approach can reduce worst-case latency.
//!
//! Furthermore, the thread pool is **adaptive**, using the least amount of threads required to "get the job done". This minimizes OS-level context switching, increasing performance in I/O bound tasks compared to the usual approach of spawning OS threads matching the number of CPUs.
//!
//! Finally, this crate has seriously minimal dependencies, and will not add significantly to your compilation times.
//!
//! This crate is heavily inspired by Stjepan Glavina's [previous work on async-std](https://async.rs/blog/stop-worrying-about-blocking-the-new-async-std-runtime/).
//!
//! `smolscale` also includes `Nursery`, a helper for [structure concurrency](https://vorpus.org/blog/notes-on-structured-concurrency-or-go-statement-considered-harmful/) on the `smolscale` global executor.

use futures_lite::prelude::*;
use once_cell::sync::OnceCell;
use std::{
    pin::Pin,
    sync::atomic::AtomicUsize,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
    time::Duration,
};
mod nursery;
pub use nursery::*;

//const CHANGE_THRESH: u32 = 10;
const MONITOR_MS: u64 = 5;

const MAX_THREADS: usize = 500;

static EXEC: async_executor::Executor<'static> = async_executor::Executor::new();

static FUTURES_BEING_POLLED: AtomicUsize = AtomicUsize::new(0);
static FBP_NONZERO: event_listener::Event = event_listener::Event::new();
static POLL_COUNT: AtomicUsize = AtomicUsize::new(0);

static THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);

static MONITOR: OnceCell<std::thread::JoinHandle<()>> = OnceCell::new();

static SINGLE_THREAD: AtomicBool = AtomicBool::new(false);

/// Irrevocably puts smolscale into single-threaded mode.
pub fn permanently_single_threaded() {
    SINGLE_THREAD.store(true, Ordering::Relaxed);
}

fn start_monitor() {
    MONITOR.get_or_init(|| {
        std::thread::Builder::new()
            .name("sscale-mon".into())
            .spawn(monitor_loop)
            .unwrap()
    });
}

fn monitor_loop() {
    fn start_thread(exitable: bool) {
        THREAD_COUNT.fetch_add(1, Ordering::SeqCst);
        std::thread::Builder::new()
            .name("sscale-wkr".into())
            .spawn(move || {
                async_io::block_on(async {
                    scopeguard::defer!({
                        THREAD_COUNT.fetch_sub(1, Ordering::SeqCst);
                    });
                    if exitable {
                        async_io::block_on(
                            EXEC.run(async_io::Timer::after(Duration::from_secs(5))),
                        );
                    } else {
                        async_io::block_on(EXEC.run(futures_lite::future::pending::<()>()));
                    };
                })
            })
            .unwrap();
    }
    start_thread(false);

    let mut consecutive_busy = 0;
    loop {
        if SINGLE_THREAD.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(MONITOR_MS));
        let fbp = loop {
            let fbp = FUTURES_BEING_POLLED.load(Ordering::SeqCst);
            if fbp > 0 {
                break fbp;
            }
            let listener = FBP_NONZERO.listen();
            let fbp = FUTURES_BEING_POLLED.load(Ordering::SeqCst);
            if fbp > 0 {
                break fbp;
            }
            consecutive_busy = 0;
            listener.wait();
        };
        let running_threads = THREAD_COUNT.load(Ordering::SeqCst);
        if fbp >= running_threads && running_threads <= MAX_THREADS {
            consecutive_busy += 1;
            if consecutive_busy > 10 {
                start_thread(true);
                consecutive_busy = 0;
            }
        } else {
            consecutive_busy = 0;
        }
    }
}

/// Spawns a future onto the global executor and immediately blocks on it.
pub fn block_on<T: Send + 'static>(future: impl Future<Output = T> + Send + 'static) -> T {
    futures_lite::future::block_on(future)
}

/// Spawns a task onto the lazily-initialized global executor.
///
/// The task can block or run CPU-intensive code if needed --- it will not block other tasks.
pub fn spawn<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> async_executor::Task<T> {
    start_monitor();
    EXEC.spawn(WrappedFuture::new(future))
    // async_global_executor::spawn(future)
}

struct WrappedFuture<T, F: Future<Output = T>> {
    fut: F,
}

static RUNNING_TASKS: AtomicUsize = AtomicUsize::new(0);

/// Returns the current number of active tasks.
pub fn active_task_count() -> usize {
    RUNNING_TASKS.load(Ordering::Relaxed)
}

impl<T, F: Future<Output = T>> Drop for WrappedFuture<T, F> {
    fn drop(&mut self) {
        RUNNING_TASKS.fetch_sub(1, Ordering::Relaxed);
    }
}

impl<T, F: Future<Output = T>> Future for WrappedFuture<T, F> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let pval = FUTURES_BEING_POLLED.fetch_add(1, Ordering::SeqCst);
        if pval == 0 {
            FBP_NONZERO.notify(1);
        }
        POLL_COUNT.fetch_add(1, Ordering::Relaxed);
        scopeguard::defer!({
            FUTURES_BEING_POLLED.fetch_sub(1, Ordering::Relaxed);
        });
        let fut = unsafe { self.map_unchecked_mut(|v| &mut v.fut) };
        fut.poll(cx)
    }
}

impl<T, F: Future<Output = T> + 'static> WrappedFuture<T, F> {
    pub fn new(fut: F) -> Self {
        RUNNING_TASKS.fetch_add(1, Ordering::Relaxed);
        WrappedFuture { fut }
    }
}
