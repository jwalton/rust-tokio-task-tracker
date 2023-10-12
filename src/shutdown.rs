pub async fn wait_for_shutdown_signal() -> std::io::Result<()> {
    implementation::wait_for_shutdown_signal().await
}

#[cfg(all(unix, not(test)))]
mod implementation {
    use std::io;
    use tokio::{
        select,
        signal::unix::{signal, SignalKind},
    };

    pub async fn wait_for_shutdown_signal() -> io::Result<()> {
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::interrupt())?;

        select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }

        Ok(())
    }
}

#[cfg(all(not(unix), not(test)))]
mod implementation {
    pub async fn wait_for_shutdown_signal() -> io::Result<()> {
        tokio::signal::ctrl_c()
    }
}

#[cfg(test)]
mod implementation {
    use std::{io, sync::{OnceLock, atomic::{AtomicUsize, Ordering}}, time::Duration};

    static WAIT_COUNT: OnceLock<AtomicUsize> = OnceLock::new();
    fn wait_count() -> &'static AtomicUsize {
        WAIT_COUNT.get_or_init(|| AtomicUsize::new(0))
    }

    static SHUTDOWN_COUNT: OnceLock<AtomicUsize> = OnceLock::new();
    fn shutdown_count() -> &'static AtomicUsize {
        SHUTDOWN_COUNT.get_or_init(|| AtomicUsize::new(0))
    }

    pub async fn wait_for_shutdown_signal() -> io::Result<()> {
        // Since we can't actually send a SIGINT or SIGTERM inside a unit test,
        // we have a `send_shutdown()` function that increments the SHUTDOWN_COUNT
        // and then `wait_for_shutdown_signal()` will bump up WAIT_COUNT and
        // then wait until SHUTDOWN_COUNT is >= WAIT_COUNT.
        wait_count().fetch_add(1, Ordering::SeqCst);
        let w = wait_count().load(Ordering::SeqCst);

        // Wait until we've seen as many shutdowns as times we've waited.
        while shutdown_count().load(Ordering::SeqCst) < w {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Ok(())
    }

    pub async fn send_shutdown() {
        shutdown_count().fetch_add(1, Ordering::SeqCst);
    }

    pub fn reset_before_test() {
        shutdown_count().store(0, Ordering::SeqCst);
        wait_count().store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
pub async fn send_shutdown() {
    implementation::send_shutdown().await;
}

#[cfg(test)]
pub fn reset_before_test() {
    implementation::reset_before_test();
}