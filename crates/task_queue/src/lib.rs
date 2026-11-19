//! Bounded batch execution.
//!
//! One batch == one UI task row. A fixed number of worker threads pull items
//! from a shared index, so memory stays bounded no matter how many files were
//! queued. Cancellation is cooperative: workers stop claiming new items, but an
//! in-flight encode finishes and cleans up its temp file.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use gimage_core::CancellationToken;

#[derive(Clone, Copy, Debug)]
pub struct BatchOptions {
    /// Heavy image jobs running in parallel. Start at 2 on ordinary machines.
    pub concurrency: usize,
    /// Retry a failed item once (I/O hiccups, locked output file).
    pub retry_failed: bool,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self { concurrency: 2, retry_failed: false }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedItem {
    pub index: usize,
    pub error: String,
}

#[derive(Clone, Debug, Default)]
pub struct BatchOutcome {
    pub total: usize,
    pub succeeded: usize,
    pub failed: Vec<FailedItem>,
    pub canceled: bool,
    pub elapsed: Duration,
}

impl BatchOutcome {
    pub fn status_label(&self) -> &'static str {
        if self.canceled {
            "已取消"
        } else if self.failed.is_empty() {
            "已完成"
        } else if self.succeeded == 0 {
            "失败"
        } else {
            "部分失败"
        }
    }
}

#[derive(Clone, Debug)]
pub enum BatchEvent {
    Started { total: usize },
    ItemStarted { index: usize },
    ItemSucceeded { index: usize },
    ItemFailed { index: usize, error: String },
    Progress { done: usize, failed: usize, total: usize },
    Finished,
}

/// Runs `work` for every item with bounded concurrency, blocking the caller.
///
/// `work` receives the item and the cancellation token; it should poll the
/// token between expensive steps and return a short, user-facing error string.
/// Events carry only item indices so the UI can map them back to its own rows.
pub fn run_batch<T, W, E>(
    items: Vec<T>,
    options: BatchOptions,
    cancel: CancellationToken,
    work: W,
    emit: E,
) -> BatchOutcome
where
    T: Send + Sync + 'static,
    W: Fn(&T, &CancellationToken) -> Result<(), String> + Send + Sync + 'static,
    E: Fn(BatchEvent) + Send + Sync + 'static,
{
    let total = items.len();
    let started = Instant::now();
    emit(BatchEvent::Started { total });

    let workers = options.concurrency.clamp(1, 16).min(total.max(1));
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::<FailedItem>::new());
    let canceled = AtomicBool::new(false);

    let items = Arc::new(items);
    let work = Arc::new(work);
    let emit = Arc::new(emit);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let items = Arc::clone(&items);
            let work = Arc::clone(&work);
            let emit = Arc::clone(&emit);
            let cancel = cancel.clone();
            let next = &next;
            let done = &done;
            let failures = &failures;
            let canceled = &canceled;
            scope.spawn(move || {
                loop {
                    if cancel.is_cancelled() {
                        canceled.store(true, Ordering::SeqCst);
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    if index >= items.len() {
                        break;
                    }
                    let item = &items[index];
                    emit(BatchEvent::ItemStarted { index });

                    let mut attempt = 0;
                    let mut last_error = String::new();
                    let succeeded = loop {
                        match work(item, &cancel) {
                            Ok(()) => break true,
                            Err(err) => {
                                last_error = err;
                                if cancel.is_cancelled() || !options.retry_failed || attempt >= 1 {
                                    break false;
                                }
                                attempt += 1;
                                std::thread::sleep(Duration::from_millis(120));
                            }
                        }
                    };

                    if succeeded {
                        emit(BatchEvent::ItemSucceeded { index });
                    } else {
                        failures
                            .lock()
                            .unwrap()
                            .push(FailedItem { index, error: last_error.clone() });
                        emit(BatchEvent::ItemFailed { index, error: last_error });
                    }
                    let done_now = done.fetch_add(1, Ordering::SeqCst) + 1;
                    let failed_now = failures.lock().unwrap().len();
                    emit(BatchEvent::Progress { done: done_now, failed: failed_now, total });
                    if cancel.is_cancelled() && done_now < total {
                        canceled.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            });
        }
    });

    let failed = failures.into_inner().unwrap();
    let succeeded = done.load(Ordering::SeqCst).saturating_sub(failed.len());
    let outcome = BatchOutcome {
        total,
        succeeded,
        failed,
        canceled: canceled.load(Ordering::SeqCst) || cancel.is_cancelled(),
        elapsed: started.elapsed(),
    };
    emit(BatchEvent::Finished);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn processes_every_item() {
        let seen = Arc::new(AtomicUsize::new(0));
        let inner = Arc::clone(&seen);
        let outcome = run_batch(
            (0..50).collect::<Vec<usize>>(),
            BatchOptions { concurrency: 4, retry_failed: false },
            CancellationToken::new(),
            move |_item, _| {
                inner.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
            |_| {},
        );
        assert_eq!(outcome.succeeded, 50);
        assert_eq!(seen.load(Ordering::SeqCst), 50);
        assert!(!outcome.canceled);
    }

    #[test]
    fn collects_failures_without_stopping() {
        let outcome = run_batch(
            (0..10).collect::<Vec<usize>>(),
            BatchOptions::default(),
            CancellationToken::new(),
            |item, _| if item % 3 == 0 { Err("boom".to_string()) } else { Ok(()) },
            |_| {},
        );
        assert_eq!(outcome.failed.len(), 4); // 0,3,6,9
        assert_eq!(outcome.succeeded, 6);
    }

    #[test]
    fn retry_once_recovers_transient_failure() {
        let attempts = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let inner = Arc::clone(&attempts);
        let outcome = run_batch(
            vec![1usize],
            BatchOptions { concurrency: 1, retry_failed: true },
            CancellationToken::new(),
            move |item, _| {
                let mut map = inner.lock().unwrap();
                let count = map.entry(*item).or_insert(0);
                *count += 1;
                if *count == 1 { Err("transient".into()) } else { Ok(()) }
            },
            |_| {},
        );
        assert_eq!(outcome.succeeded, 1);
        assert!(outcome.failed.is_empty());
    }

    #[test]
    fn cancellation_stops_new_items() {
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let ran = Arc::new(AtomicUsize::new(0));
        let inner = Arc::clone(&ran);
        let outcome = run_batch(
            (0..500).collect::<Vec<usize>>(),
            BatchOptions { concurrency: 2, retry_failed: false },
            cancel.clone(),
            move |_item, _| {
                inner.fetch_add(1, Ordering::SeqCst);
                token.cancel();
                Ok(())
            },
            |_| {},
        );
        assert!(outcome.canceled);
        assert!(ran.load(Ordering::SeqCst) < 500);
        assert_eq!(outcome.total, 500);
    }
}