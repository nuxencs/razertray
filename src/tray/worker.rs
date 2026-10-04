//! Background thread that polls devices and sends the results to the event loop.

use super::UserEvent;
use crate::config::PidCache;
use crate::hid::client;
use crate::model::PollResult;
use hidapi::HidApi;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;
use tao::event_loop::EventLoopProxy;
use tracing::{Level, event};

/// First retry delay after a poll that finds no device. It doubles on each
/// further empty poll, up to the poll interval.
const FIRST_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug)]
enum Command {
    Refresh,
    Exit,
}

/// Handle to the poll thread. The thread stops when this is dropped.
#[derive(Debug)]
pub(super) struct Worker {
    commands: mpsc::Sender<Command>,
}

impl Worker {
    /// Starts polling at once, then after each `interval`.
    pub(super) fn spawn(
        proxy: EventLoopProxy<UserEvent>,
        cache: PidCache,
        interval: Duration,
    ) -> Self {
        let (commands, receiver) = mpsc::channel();
        thread::spawn(move || poll_loop(&proxy, &receiver, cache, interval));
        Self { commands }
    }

    /// Polls now instead of at the next interval.
    pub(super) fn refresh(&self) {
        // A send error means the thread is gone; nothing is left to refresh.
        let _ = self.commands.send(Command::Refresh);
    }

    pub(super) fn stop(&self) {
        let _ = self.commands.send(Command::Exit);
    }
}

fn poll_loop(
    proxy: &EventLoopProxy<UserEvent>,
    commands: &mpsc::Receiver<Command>,
    mut cache: PidCache,
    interval: Duration,
) {
    let mut backoff = Backoff::new(interval);
    // Created once and reused; `refresh_devices` picks up attached and removed
    // devices. Created again on the next poll if it fails to start.
    let mut api: Option<HidApi> = None;

    loop {
        if api.is_none() {
            match HidApi::new() {
                Ok(new_api) => api = Some(new_api),
                Err(err) => {
                    event!(
                        name: "hidapi.init.failure",
                        Level::WARN,
                        exception.message = %err,
                        "cannot start hidapi: {{exception.message}}",
                    );
                }
            }
        }

        let cache_before = cache.clone();
        let result = match api.as_mut() {
            Some(api) => {
                if let Err(err) = api.refresh_devices() {
                    event!(
                        name: "hidapi.refresh.failure",
                        Level::WARN,
                        exception.message = %err,
                        "cannot refresh the HID device list: {{exception.message}}",
                    );
                }
                client::poll_devices(api, &mut cache)
            }
            None => PollResult::default(),
        };

        // The cache only changes when a new device was probed.
        if cache != cache_before
            && let Err(err) = cache.save()
        {
            event!(
                name: "pid_cache.save.failure",
                Level::WARN,
                exception.message = %format_args!("{err:#}"),
                "cannot save the pid cache: {{exception.message}}",
            );
        }

        let wait = if result.devices.is_empty() {
            backoff.after_miss()
        } else {
            backoff.after_hit()
        };
        if proxy.send_event(UserEvent::Poll(result)).is_err() {
            break; // The event loop is gone.
        }

        match commands.recv_timeout(wait) {
            Ok(Command::Refresh) | Err(RecvTimeoutError::Timeout) => {}
            Ok(Command::Exit) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Wait before the next poll. A device that answers gets the normal interval.
/// When nothing answers, retries come quickly and then slow down, so a mouse
/// that wakes up is found fast without polling the USB bus all the time.
#[derive(Debug)]
struct Backoff {
    interval: Duration,
    next_retry: Duration,
}

impl Backoff {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_retry: FIRST_RETRY.min(interval),
        }
    }

    fn after_hit(&mut self) -> Duration {
        self.next_retry = FIRST_RETRY.min(self.interval);
        self.interval
    }

    fn after_miss(&mut self) -> Duration {
        let wait = self.next_retry;
        self.next_retry = (wait * 2).min(self.interval);
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::Backoff;
    use std::time::Duration;

    fn secs(waits: &[Duration]) -> Vec<u64> {
        waits.iter().map(Duration::as_secs).collect()
    }

    #[test]
    fn misses_double_up_to_the_interval() {
        let mut backoff = Backoff::new(Duration::from_secs(20));
        let waits: Vec<Duration> = (0..6).map(|_| backoff.after_miss()).collect();
        assert_eq!(secs(&waits), [2, 4, 8, 16, 20, 20]);
    }

    #[test]
    fn a_hit_resets_the_retry_delay() {
        let mut backoff = Backoff::new(Duration::from_secs(60));
        backoff.after_miss();
        backoff.after_miss();
        assert_eq!(backoff.after_hit(), Duration::from_secs(60));
        assert_eq!(backoff.after_miss(), Duration::from_secs(2));
    }

    #[test]
    fn short_interval_caps_the_first_retry() {
        let mut backoff = Backoff::new(Duration::from_secs(1));
        assert_eq!(backoff.after_miss(), Duration::from_secs(1));
    }
}
