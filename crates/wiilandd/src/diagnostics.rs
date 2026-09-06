//! Bounded, best-effort diagnostic output. No writer runs on the input reactor.
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

const QUEUE_RECORDS: usize = 256;
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub(crate) struct DiagnosticSender {
    sender: SyncSender<String>,
    dropped: Arc<AtomicU64>,
}

impl DiagnosticSender {
    pub(crate) fn send(&self, line: &str) {
        if line.len() > MAX_RECORD_BYTES || self.sender.try_send(line.to_owned()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

pub(crate) struct DiagnosticWriter {
    sender: Option<DiagnosticSender>,
    finished: Receiver<()>,
}

impl DiagnosticWriter {
    pub(crate) fn stdout() -> Self {
        Self::spawn(io::stdout)
    }

    pub(crate) fn stderr() -> Self {
        Self::spawn(io::stderr)
    }

    fn spawn<W: Write + 'static>(writer: impl FnOnce() -> W + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<String>(QUEUE_RECORDS);
        let (done, finished) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&dropped);
        crate::signal::spawn_worker("wiiland-diagnostics", move || {
            let mut writer = writer();
            let mut reported = 0;
            for line in receiver {
                let current = counter.load(Ordering::Relaxed);
                if current != reported {
                    if writeln!(
                        writer,
                        "wiilandd: diagnostics: dropped {} records",
                        current - reported
                    )
                    .is_err()
                    {
                        break;
                    }
                    reported = current;
                }
                if writeln!(writer, "{line}")
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break;
                }
            }
            let _ = done.send(());
        })
        .expect("cannot start diagnostic writer");
        Self {
            sender: Some(DiagnosticSender { sender, dropped }),
            finished,
        }
    }

    pub(crate) fn sender(&self) -> DiagnosticSender {
        self.sender.as_ref().expect("writer is open").clone()
    }
}

impl Drop for DiagnosticWriter {
    fn drop(&mut self) {
        self.sender.take();
        // A pipe reader may stop forever. Shutdown must still release devices.
        let _ = self.finished.recv_timeout(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stalled_writer_has_bounded_queue_and_does_not_block_producer() {
        let (release, wait) = mpsc::channel();
        let writer = DiagnosticWriter::spawn(move || {
            wait.recv().unwrap();
            io::sink()
        });
        let sender = writer.sender();
        for _ in 0..QUEUE_RECORDS + 3 {
            sender.send("event");
        }
        assert_eq!(sender.dropped(), 3);
        sender.send(&"x".repeat(MAX_RECORD_BYTES + 1));
        assert_eq!(sender.dropped(), 4);
        drop(sender);
        release.send(()).unwrap();
        drop(writer);
    }

    #[test]
    fn closed_consumer_counts_loss_without_panicking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(receiver);
        let sender = DiagnosticSender {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        sender.send("event");
        assert_eq!(sender.dropped(), 1);
    }
}
