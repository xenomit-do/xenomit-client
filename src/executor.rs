use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use async_task::Runnable;
use ruffle_core::backend::navigator::OwnedFuture;
use ruffle_core::loader::Error;

/// Ruffle futures stay on the player thread; their wakers may run on network threads.
pub struct GameExecutor {
    ready: Receiver<Runnable>,
    sender: Sender<Runnable>,
    pending: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
    last_report: Instant,
    polls_since_report: u64,
}

#[derive(Clone)]
pub struct GameSpawner {
    sender: Sender<Runnable>,
    pending: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

struct PendingGuard {
    pending: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.fetch_sub(1, Ordering::Relaxed);
        self.finished.fetch_add(1, Ordering::Relaxed);
    }
}

impl GameExecutor {
    pub fn new() -> Self {
        let (sender, ready) = mpsc::channel();
        Self {
            ready,
            sender,
            pending: Arc::new(AtomicUsize::new(0)),
            finished: Arc::new(AtomicUsize::new(0)),
            last_report: Instant::now(),
            polls_since_report: 0,
        }
    }

    pub fn spawner(&self) -> GameSpawner {
        GameSpawner {
            sender: self.sender.clone(),
            pending: Arc::clone(&self.pending),
            finished: Arc::clone(&self.finished),
        }
    }

    pub fn run(&mut self) {
        if self.last_report.elapsed() >= Duration::from_secs(10) {
            eprintln!(
                "[executor] pending Flash tasks: {}, polls: {}, completions: {}",
                self.pending.load(Ordering::Relaxed),
                self.polls_since_report,
                self.finished.swap(0, Ordering::Relaxed)
            );
            self.polls_since_report = 0;
            self.last_report = Instant::now();
        }

        let deadline = Instant::now() + Duration::from_millis(4);
        // Bound work even if a future wakes itself every time it is polled.
        for _ in 0..128 {
            let Ok(runnable) = self.ready.try_recv() else {
                break;
            };
            self.polls_since_report += 1;
            runnable.run();
            if Instant::now() >= deadline {
                break;
            }
        }
    }
}

impl GameSpawner {
    pub fn spawn(&self, future: OwnedFuture<(), Error>) {
        let sender = self.sender.clone();
        let pending = Arc::clone(&self.pending);
        pending.fetch_add(1, Ordering::Relaxed);
        let pending_guard = PendingGuard {
            pending,
            finished: Arc::clone(&self.finished),
        };
        let future = async move {
            let _pending_guard = pending_guard;
            if let Err(error) = future.await {
                tracing::error!("Asynchronous Flash load failed: {error}");
            }
        };
        let schedule = move |runnable| {
            let _ = sender.send(runnable);
        };
        let (runnable, task) = async_task::Builder::new().spawn_local(|_| future, schedule);
        task.detach();
        runnable.schedule();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Poll, Waker};

    #[test]
    fn continuously_ready_task_cannot_hold_window_loop() {
        let mut executor = GameExecutor::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&polls);
        executor
            .spawner()
            .spawn(Box::pin(std::future::poll_fn(move |context| {
                counter.fetch_add(1, Ordering::Relaxed);
                context.waker().wake_by_ref();
                Poll::Pending
            })));
        executor.run();
        assert!(polls.load(Ordering::Relaxed) <= 128);
        assert!(polls.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn network_thread_wake_reaches_executor() {
        let mut executor = GameExecutor::new();
        let polls = Arc::new(AtomicUsize::new(0));
        let saved_waker = Arc::new(Mutex::new(None::<Waker>));
        let count = Arc::clone(&polls);
        let waker = Arc::clone(&saved_waker);
        executor
            .spawner()
            .spawn(Box::pin(std::future::poll_fn(move |context| {
                let poll = count.fetch_add(1, Ordering::Relaxed);
                *waker.lock().unwrap() = Some(context.waker().clone());
                if poll == 0 {
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            })));
        executor.run();
        executor.run();
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        let waker = saved_waker.lock().unwrap().as_ref().unwrap().clone();
        std::thread::spawn(move || waker.wake()).join().unwrap();
        executor.run();
        assert_eq!(polls.load(Ordering::Relaxed), 2);
        assert_eq!(executor.pending.load(Ordering::Relaxed), 0);
    }
}
