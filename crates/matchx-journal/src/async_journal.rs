use crate::{JournalError, JournalWriter, codec};
use matchx_types::Command;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// Runtime configuration for the async journal pipeline.
#[derive(Debug, Clone)]
pub struct AsyncJournalConfig {
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub flush_interval_ms: u64,
    pub segment_max_bytes: u64,
}

impl Default for AsyncJournalConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 1024,
            batch_size: 64,
            flush_interval_ms: 10,
            segment_max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Async journal front-door. Background worker wiring is added in later tasks.
pub struct AsyncJournal {
    _path_prefix: PathBuf,
    _cfg: AsyncJournalConfig,
    tx: SyncSender<WorkerMessage>,
    worker: Mutex<Option<JoinHandle<()>>>,
    accepted: AtomicU64,
    durable: Arc<AtomicU64>,
    degraded: Arc<AtomicBool>,
    closed: AtomicBool,
}

impl AsyncJournal {
    pub fn open(
        path_prefix: impl AsRef<Path>,
        cfg: AsyncJournalConfig,
    ) -> Result<Self, JournalError> {
        let path_prefix = path_prefix.as_ref().to_path_buf();
        if let Some(parent) = path_prefix.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        let segment_path = first_segment_path(&path_prefix);
        let writer = JournalWriter::open(&segment_path)?;

        let (tx, rx) = sync_channel(cfg.queue_capacity);
        let durable = Arc::new(AtomicU64::new(0));
        let worker_durable = Arc::clone(&durable);
        let degraded = Arc::new(AtomicBool::new(false));
        let worker_degraded = Arc::clone(&degraded);
        let batch_size = cfg.batch_size.max(1);
        let worker = thread::spawn(move || {
            worker_loop(writer, rx, batch_size, worker_durable, worker_degraded)
        });

        Ok(Self {
            _path_prefix: path_prefix,
            _cfg: cfg,
            tx,
            worker: Mutex::new(Some(worker)),
            accepted: AtomicU64::new(0),
            durable,
            degraded,
            closed: AtomicBool::new(false),
        })
    }

    pub fn append(&self, sequence: u64, cmd: &Command) -> Result<(), JournalError> {
        if self.degraded.load(Ordering::Acquire) {
            return Err(JournalError::WriterDegraded);
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(JournalError::WriterStopped);
        }

        let record = AppendRecord {
            sequence,
            bytes: codec::encode_record(sequence, cmd),
        };

        self.tx
            .try_send(WorkerMessage::Append(record))
            .map_err(|e| match e {
                TrySendError::Full(_) => JournalError::QueueFull,
                TrySendError::Disconnected(_) => JournalError::WriterStopped,
            })?;

        self.accepted.store(sequence, Ordering::Release);
        Ok(())
    }

    pub fn accepted_sequence(&self) -> u64 {
        self.accepted.load(Ordering::Acquire)
    }

    pub fn durable_sequence(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }

    pub fn close(&self) -> Result<(), JournalError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let _ = self.tx.send(WorkerMessage::Shutdown);

        if let Some(handle) = self.worker.lock().unwrap().take() {
            let _ = handle.join();
        }

        Ok(())
    }

    #[cfg(test)]
    fn trigger_writer_io_error_for_test(&self) {
        let _ = self.tx.try_send(WorkerMessage::InjectFailure);
    }
}

impl Drop for AsyncJournal {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn first_segment_path(path_prefix: &Path) -> PathBuf {
    path_prefix.with_extension("wal")
}

enum WorkerMessage {
    Append(AppendRecord),
    Shutdown,
    #[cfg(test)]
    InjectFailure,
}

struct AppendRecord {
    sequence: u64,
    bytes: Vec<u8>,
}

fn worker_loop(
    mut writer: JournalWriter,
    rx: Receiver<WorkerMessage>,
    batch_size: usize,
    durable: Arc<AtomicU64>,
    degraded: Arc<AtomicBool>,
) {
    while let Ok(msg) = rx.recv() {
        let first_msg = msg;

        let mut batch = Vec::with_capacity(batch_size);
        let mut should_exit = false;

        let mut last_sequence = match first_msg {
            WorkerMessage::Append(record) => {
                batch.push(record.bytes);
                record.sequence
            }
            WorkerMessage::Shutdown => break,
            #[cfg(test)]
            WorkerMessage::InjectFailure => {
                degraded.store(true, Ordering::Release);
                break;
            }
        };

        while batch.len() < batch_size {
            match rx.try_recv() {
                Ok(WorkerMessage::Append(record)) => {
                    last_sequence = record.sequence;
                    batch.push(record.bytes);
                }
                Ok(WorkerMessage::Shutdown) => {
                    should_exit = true;
                    break;
                }
                #[cfg(test)]
                Ok(WorkerMessage::InjectFailure) => {
                    degraded.store(true, Ordering::Release);
                    should_exit = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    should_exit = true;
                    break;
                }
            }
        }

        if !batch.is_empty() {
            if writer.append_raw_batch(&batch).is_err() {
                degraded.store(true, Ordering::Release);
                break;
            }
            durable.store(last_sequence, Ordering::Release);
        }

        if should_exit {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AsyncJournal, AsyncJournalConfig};
    use crate::JournalError;
    use matchx_types::{Command, OrderId};
    use std::time::{Duration, Instant};

    #[test]
    fn async_journal_exposes_accepted_and_durable_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("seg");
        let j = AsyncJournal::open(prefix, AsyncJournalConfig::default()).unwrap();
        assert_eq!(j.accepted_sequence(), 0);
        assert_eq!(j.durable_sequence(), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // Miri thread scheduling may drain the queue before second append
    fn append_returns_queue_full_when_capacity_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("seg");
        let cfg = AsyncJournalConfig {
            queue_capacity: 1,
            ..Default::default()
        };
        let j = AsyncJournal::open(prefix, cfg).unwrap();

        j.append(1, &cmd()).unwrap();
        let err = j.append(2, &cmd()).unwrap_err();
        assert!(matches!(err, JournalError::QueueFull));
    }

    #[test]
    fn durable_sequence_catches_up_after_background_flush() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("seg");
        let j = AsyncJournal::open(prefix, AsyncJournalConfig::default()).unwrap();

        j.append(1, &cmd()).unwrap();
        j.append(2, &cmd()).unwrap();
        wait_until(|| j.durable_sequence() >= 2, Duration::from_millis(250));
        assert_eq!(j.accepted_sequence(), 2);
    }

    #[test]
    fn append_fails_fast_after_writer_enters_degraded_state() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("seg");
        let j = AsyncJournal::open(prefix, AsyncJournalConfig::default()).unwrap();

        trigger_writer_io_error(&j);
        wait_until(|| j.is_degraded(), Duration::from_millis(250));
        assert!(matches!(
            j.append(10, &cmd()),
            Err(JournalError::WriterDegraded)
        ));
    }

    fn cmd() -> Command {
        Command::CancelOrder { id: OrderId(42) }
    }

    fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition not met within {:?}", timeout);
    }

    fn trigger_writer_io_error(journal: &AsyncJournal) {
        journal.trigger_writer_io_error_for_test();
    }
}
