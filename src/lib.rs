//! tokio-task-tracker is a simple graceful shutdown solution for tokio.
//!
//! The basic idea is to use a `TaskSpawner` to create `TaskTracker` object, and hold
//! on to them in spawned tasks. Inside the task, you can check `tracker.cancelled().await`
//! to wait for the task to be cancelled.
//!
//! The `TaskWaiter` can be used to wait for an interrupt and then wait for all
//! `TaskTracker`s to be dropped.
//!
//! # Examples
//!
//! ```no_run
//! # use std::time::Duration;
//! #
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let (spawner, waiter) = tokio_task_tracker::new();
//!
//!     // Start a task
//!     spawner.spawn(|tracker| async move {
//!         tokio::select! {
//!             _ = tracker.cancelled() => {
//!                 // The token was cancelled, task should shut down.
//!             }
//!             _ = tokio::time::sleep(Duration::from_secs(9999)) => {
//!                 // Long work has completed
//!             }
//!         }
//!     });
//!
//!     // Wait for all tasks to complete, or for someone to hit ctrl-c.
//!     // If tasks down't complete within 5 seconds, we'll quit anyways.
//!     waiter.wait_for_shutdown(Duration::from_secs(5)).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! If you do not wish to allow a task to be aborted, you still need to make sure
//! the task captures the tracker, because TaskWaiter will wait for all trackers to be dropped:
//!
//! ```no_run
//! # use std::time::Duration;
//! #
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! #     let (spawner, waiter) = tokio_task_tracker::new();
//! #
//!     // Start a task
//!     spawner.spawn(|tracker| async move {
//!         // Move the tracker into the task.
//!         let _tracker = tracker;
//!
//!         // Do some work that we don't want to abort.
//!         tokio::time::sleep(Duration::from_secs(9999)).await;
//!     });
//!
//! # }
//! ```
//!
//! You can also create a tracker via the `task` method:
//!
//! ```no_run
//! # use std::time::Duration;
//! #
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! #     let (spawner, waiter) = tokio_task_tracker::new();
//! #
//!     // Start a task
//!     let tracker = spawner.task();
//!     tokio::task::spawn(async move {
//!         // Move the tracker into the task.
//!         let _tracker = tracker;
//!
//!         // ...
//!     });
//!
//! # }
//! ```
//!
//! Trackers can be used to spawn subtasks via `tracker.subtask()` or
//! `tracker.spawn()`.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use shutdown::wait_for_shutdown_signal;
use tokio::{select, sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

mod shutdown;

/// Builder is used to create a TaskSpawner and TaskWaiter.
pub struct Builder {
    token: Option<CancellationToken>,
}

/// TaskSpawner is used to spawn new task trackers.
#[derive(Clone)]
pub struct TaskSpawner {
    token: CancellationToken,
    stop_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}

/// TaskWaiter is used to wait until all task trackers have been dropped.
pub struct TaskWaiter {
    token: CancellationToken,
    /// Shared stop_tx is shared between all TaskSpawners and the TaskWaiter, so that
    /// when we call TaskWaiter::wait() we can drop the tx from all spawners.
    stop_tx: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    stop_rx: mpsc::Receiver<()>,
}

/// A TaskTracker is used both as a token to keep track of active tasks, and
/// as a cancellation token to check to see if the current task should quit.
#[derive(Clone)]
pub struct TaskTracker {
    token: CancellationToken,
    // Hang on to an instance of tx. We do this so we can know when all tasks
    // have been completed.
    _stop_tx: Option<mpsc::Sender<()>>,
}

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Returned when we timeout waiting for all tasks to shut down.
    Timeout,
    /// Returned when we cannot bind to the interrupt/terminate signals.
    CouldNotBindInterrupt,
    /// Returned when we were waiting for graceful shutdown, but received a
    /// second interrupt signal.
    ShutdownEarly,
}

impl std::error::Error for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Timeout => write!(f, "Not all tasks finished before timeout"),
            Error::CouldNotBindInterrupt => write!(f, "Could not bind interrupt handler"),
            Error::ShutdownEarly => write!(f, "Skipping graceful shutdown due to second interrupt"),
        }
    }
}

/// Create a new TaskSpawner and TaskWaiter.
pub fn new() -> (TaskSpawner, TaskWaiter) {
    Builder::default().build()
}

impl Builder {
    /// Create a new Builder.
    pub fn new() -> Self {
        Builder { token: None }
    }

    /// Use an existing CancellationToken for the returned TaskWaiter and TaskSpawner.
    /// If the given token is cancelled, all associated TaskTrackers will be cancelled
    /// as well.
    pub fn set_cancellation_token(mut self, token: CancellationToken) -> Self {
        self.token = Some(token);
        self
    }

    /// Create a new TaskSpawner and TaskWaiter.
    pub fn build(self) -> (TaskSpawner, TaskWaiter) {
        let (stop_tx, stop_rx) = mpsc::channel(1);
        let stop_tx = Arc::new(Mutex::new(Some(stop_tx)));
        let token = self.token.unwrap_or(CancellationToken::new());

        (
            TaskSpawner {
                token: token.clone(),
                stop_tx: stop_tx.clone(),
            },
            TaskWaiter {
                token,
                stop_tx,
                stop_rx,
            },
        )
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskSpawner {
    /// Create a new TaskTracker.
    pub fn task(&self) -> TaskTracker {
        TaskTracker {
            token: self.token.clone(),
            _stop_tx: self.stop_tx.lock().unwrap().as_ref().cloned(),
        }
    }

    /// Spawn a task.
    ///
    /// The given closure will be called, passing in a task tracker.
    pub fn spawn<T, F: FnOnce(TaskTracker) -> T>(&self, f: F) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        let tracker = self.task();
        tokio::task::spawn(f(tracker))
    }

    /// Notify all tasks created by this TaskSpawner that they should abort.
    pub fn cancel(&self) {
        self.token.cancel();
    }
}

impl TaskWaiter {
    /// Notify all tasks this TaskWaiter is waiting on that they should abort.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Wait for the application to be interrupted, and then gracefully shutdown
    /// allowing a timeout for all tasks to quit.  A second interrupt will cause
    /// an immediate shutdown.
    ///
    /// On Unix systems, "interrupt" means a SIGINT or SIGTERM. On all other
    /// platforms the current implementation uses `tokio::signal::ctrl_c()`
    /// to wait for an interrupt.
    pub async fn wait_for_shutdown(self, timeout: Duration) -> Result<(), Error> {
        // Wait for the ctrl-c.
        match wait_for_shutdown_signal().await {
            Ok(()) => {
                // time to shut down...
            }
            Err(_) => return Err(Error::CouldNotBindInterrupt),
        }

        // Let tasks know they should shut down.
        self.token.cancel();

        // Wait for everything to finish.
        select! {
            res = self.wait_with_timeout(timeout) => res,
            _ = wait_for_shutdown_signal() => Err(Error::ShutdownEarly),
        }
    }

    /// Wait for all tasks to finish.  If tasks do not finish before the timeout,
    /// `Error::Timeout` will be returned.
    pub async fn wait_with_timeout(self, timeout: Duration) -> Result<(), Error> {
        // Wait for all tasks to be dropped.
        tokio::time::timeout(timeout, self.wait())
            .await
            .map_err(|_| Error::Timeout {})?;

        Ok(())
    }

    /// Wait for all tasks to finish.
    pub async fn wait(mut self) {
        // Drop the tx half of the channel.
        drop(self.stop_tx.lock().unwrap().take());

        // Wait for all tasks to be dropped.
        let _ = self.stop_rx.recv().await;
    }
}

impl TaskTracker {
    /// Create a new subtask from this TaskTracker.
    pub fn subtask(&self) -> Self {
        self.clone()
    }

    /// Spawn a subtask.
    ///
    /// The given closure will be called, passing in a task tracker.
    pub fn spawn<T, F: FnOnce(TaskTracker) -> T>(&self, f: F) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        let tracker = self.subtask();
        tokio::task::spawn(f(tracker))
    }

    /// Check to see if this task has been cancelled.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// Returns true if this token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    };

    #[tokio::test]
    async fn tracker_should_be_cancelled() {
        let (spawner, waiter) = super::new();

        let task = spawner.task();
        waiter.cancel();
        assert!(task.is_cancelled());
    }

    #[tokio::test]
    async fn should_work_with_existing_cancellation_token() {
        let token = CancellationToken::new();
        let (spawner, _) = super::Builder::new()
            .set_cancellation_token(token.clone())
            .build();
        let task = spawner.task();

        // Cancelling the token should cancel the task.
        token.cancel();
        assert!(task.is_cancelled());
    }

    #[tokio::test]
    async fn should_wait_for_tasks_to_complete() -> Result<(), Box<dyn std::error::Error>> {
        let (spawner, waiter) = super::new();

        let done = Arc::new(AtomicBool::new(false));

        // Start a task
        {
            let done = done.clone();
            spawner.spawn(|tracker| async move {
                tokio::select! {
                    _ = tracker.cancelled() => {
                        // The token was cancelled, task should shut down.
                    }
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        // Short task has completed.
                        done.store(true, Ordering::SeqCst);
                    }
                }
            });
        }

        // Wait for all tasks to complete.
        waiter.wait().await;

        // Should have completed.
        assert!(done.load(Ordering::SeqCst));

        Ok(())
    }

    #[tokio::test]
    async fn should_cancel_tasks() -> Result<(), Box<dyn std::error::Error>> {
        let (spawner, waiter) = super::new();

        let done = Arc::new(AtomicBool::new(false));

        // Start a task
        {
            let done = done.clone();
            spawner.spawn(|tracker| async move {
                tokio::select! {
                    _ = tracker.cancelled() => {
                        // The token was cancelled, task should shut down.
                    }
                    _ = tokio::time::sleep(Duration::from_secs(9999)) => {
                        // Long work has completed
                        done.store(true, Ordering::SeqCst);
                    }
                }
            });
        }

        // Cancel the task after a short while.
        tokio::time::sleep(Duration::from_millis(100)).await;
        waiter.cancel();

        // Wait for all tasks to complete.
        waiter.wait().await;

        // Should have timed out.
        assert!(!done.load(Ordering::SeqCst));

        Ok(())
    }

    #[tokio::test]
    async fn interrupt_tests() -> Result<(), Box<dyn std::error::Error>> {
        // Interrupt tests rely on global state in shutdown.rs to simulate
        // SIGINT.  Need to run these serially.
        should_wait_for_tasks_on_interrupt().await?;
        should_stop_immediately_on_second_interrupt().await?;

        Ok(())
    }

    async fn should_wait_for_tasks_on_interrupt() -> Result<(), Box<dyn std::error::Error>> {
        shutdown::reset_before_test();

        let (spawner, waiter) = super::new();

        let done = Arc::new(AtomicBool::new(false));

        // Start a task
        {
            let done = done.clone();
            spawner.spawn(|tracker| async move {
                tokio::select! {
                    _ = tracker.cancelled() => {
                        // The token was cancelled, task should shut down.
                    }
                    _ = tokio::time::sleep(Duration::from_secs(9999)) => {
                        // Long running task...
                        done.store(true, Ordering::SeqCst);
                    }
                }
            });
        }

        // Send a fake shutdown signal.
        tokio::spawn(async {
            shutdown::send_shutdown().await;
        });

        // Wait for all tasks to complete.
        waiter.wait_for_shutdown(Duration::from_secs(10)).await?;

        // Task should have been aborted.
        assert!(!done.load(Ordering::SeqCst));

        Ok(())
    }

    async fn should_stop_immediately_on_second_interrupt() -> Result<(), Box<dyn std::error::Error>>
    {
        shutdown::reset_before_test();

        let (spawner, waiter) = super::new();

        let done = Arc::new(AtomicBool::new(false));

        // Start a task
        {
            let done = done.clone();
            spawner.spawn(|tracker| async move {
                let _tracker = tracker;

                // Long running task that can't be cancelled.
                tokio::time::sleep(Duration::from_secs(99)).await;
                done.store(true, Ordering::SeqCst);
            });
        }

        // Send two shutdown signals. The second should cause us to die immediately.
        tokio::spawn(async move {
            shutdown::send_shutdown().await;
            shutdown::send_shutdown().await;
        });

        // We shouldn't wait here, because of the second interrupt.
        let err = waiter
            .wait_for_shutdown(Duration::from_secs(99))
            .await
            .unwrap_err();
        assert_eq!(err, Error::ShutdownEarly);

        // Task should have been aborted.
        assert!(!done.load(Ordering::SeqCst));

        Ok(())
    }
}
