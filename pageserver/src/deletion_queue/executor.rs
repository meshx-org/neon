use remote_storage::GenericRemoteStorage;
use remote_storage::RemotePath;
use remote_storage::MAX_KEYS_PER_DELETE;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing::warn;

use crate::metrics::DELETION_QUEUE_ERRORS;
use crate::metrics::DELETION_QUEUE_EXECUTED;

use super::DeletionQueueError;
use super::FlushOp;

const AUTOFLUSH_INTERVAL: Duration = Duration::from_secs(10);

pub(super) enum ExecutorMessage {
    Delete(Vec<RemotePath>),
    Flush(FlushOp),
}

/// Non-persistent deletion queue, for coalescing multiple object deletes into
/// larger DeleteObjects requests.
pub struct ExecutorWorker {
    // Accumulate up to 1000 keys for the next deletion operation
    accumulator: Vec<RemotePath>,

    rx: tokio::sync::mpsc::Receiver<ExecutorMessage>,

    cancel: CancellationToken,
    remote_storage: GenericRemoteStorage,
}

impl ExecutorWorker {
    pub(super) fn new(
        remote_storage: GenericRemoteStorage,
        rx: tokio::sync::mpsc::Receiver<ExecutorMessage>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            remote_storage,
            rx,
            cancel,
            accumulator: Vec::new(),
        }
    }

    /// Wrap the remote `delete_objects` with a failpoint
    pub async fn remote_delete(&self) -> Result<(), anyhow::Error> {
        fail::fail_point!("deletion-queue-before-execute", |_| {
            info!("Skipping execution, failpoint set");
            DELETION_QUEUE_ERRORS
                .with_label_values(&["failpoint"])
                .inc();
            return Err(anyhow::anyhow!("failpoint hit"));
        });

        self.remote_storage.delete_objects(&self.accumulator).await
    }

    /// Block until everything in accumulator has been executed
    pub async fn flush(&mut self) -> Result<(), DeletionQueueError> {
        while !self.accumulator.is_empty() && !self.cancel.is_cancelled() {
            match self.remote_delete().await {
                Ok(()) => {
                    // Note: we assume that the remote storage layer returns Ok(()) if some
                    // or all of the deleted objects were already gone.
                    DELETION_QUEUE_EXECUTED.inc_by(self.accumulator.len() as u64);
                    info!(
                        "Executed deletion batch {}..{}",
                        self.accumulator
                            .first()
                            .expect("accumulator should be non-empty"),
                        self.accumulator
                            .last()
                            .expect("accumulator should be non-empty"),
                    );
                    self.accumulator.clear();
                }
                Err(e) => {
                    warn!("DeleteObjects request failed: {e:#}, will retry");
                    DELETION_QUEUE_ERRORS.with_label_values(&["execute"]).inc();
                }
            };
        }
        if self.cancel.is_cancelled() {
            // Expose an error because we may not have actually flushed everything
            Err(DeletionQueueError::ShuttingDown)
        } else {
            Ok(())
        }
    }

    pub async fn background(&mut self) -> Result<(), DeletionQueueError> {
        self.accumulator.reserve(MAX_KEYS_PER_DELETE);

        loop {
            if self.cancel.is_cancelled() {
                return Err(DeletionQueueError::ShuttingDown);
            }

            let msg = match tokio::time::timeout(AUTOFLUSH_INTERVAL, self.rx.recv()).await {
                Ok(Some(m)) => m,
                Ok(None) => {
                    // All queue senders closed
                    info!("Shutting down");
                    return Err(DeletionQueueError::ShuttingDown);
                }
                Err(_) => {
                    // Timeout, we hit deadline to execute whatever we have in hand.  These functions will
                    // return immediately if no work is pending
                    self.flush().await?;

                    continue;
                }
            };

            match msg {
                ExecutorMessage::Delete(mut list) => {
                    while !list.is_empty() || self.accumulator.len() == MAX_KEYS_PER_DELETE {
                        if self.accumulator.len() == MAX_KEYS_PER_DELETE {
                            self.flush().await?;
                            // If we have received this number of keys, proceed with attempting to execute
                            assert_eq!(self.accumulator.len(), 0);
                        }

                        let available_slots = MAX_KEYS_PER_DELETE - self.accumulator.len();
                        let take_count = std::cmp::min(available_slots, list.len());
                        for path in list.drain(list.len() - take_count..) {
                            self.accumulator.push(path);
                        }
                    }
                }
                ExecutorMessage::Flush(flush_op) => {
                    // If flush() errors, we drop the flush_op and the caller will get
                    // an error recv()'ing their oneshot channel.
                    self.flush().await?;
                    flush_op.fire();
                }
            }
        }
    }
}
