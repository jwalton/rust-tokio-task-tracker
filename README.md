# tokio-task-tracker

tokio-task-tracker is a simple graceful shutdown solution for tokio.

The basic idea is to use a `TaskSpawner` to create `TaskTracker` objects, and hold
on to them in spawned tasks. Inside the task, you can check `tracker.cancelled().await`
to wait for the task to be cancelled.

The `TaskWaiter` can be used to wait for an interrupt and then wait for all
trackers to be dropped.

## Example

```rust
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (spawner, waiter) = tokio_task_tracker::new();

    // Start a task
    spawner.spawn(|tracker| async move {
        tokio::select! {
            _ = tracker.cancelled() => {
                // The token was cancelled, task should shut down.
            }
            _ = tokio::time::sleep(Duration::from_secs(9999)) => {
                // Long work has completed
            }
        }
    });

    // Wait for all tasks to complete, or for someone to hit ctrl-c.
    // If tasks down't complete within 5 seconds, we'll quit anyways.
    waiter.wait_for_shutdown(Duration::from_secs(5)).await?;

    Ok(())
}
```

If you do not wish to allow a task to be aborted, you still need to make sure the task captures the tracker:

```rust
    // Start a task
    spawner.spawn(|tracker| async move {
        // Move the tracker into the task.
        let _tracker = tracker;

        // Do some work that we don't want to abort.
        tokio::time::sleep(Duration::from_secs(9999)).await;
    });
```
