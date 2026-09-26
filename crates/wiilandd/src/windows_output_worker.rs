use crate::bridge::windows_output::WindowsOutputSession;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use wiiland_core::engine::OutputAction;

const COMMAND_CAPACITY: usize = 256;
const TAG_SHIFT: u32 = 32;
const TAG_MASK: u64 = 0xffff_ffff_0000_0000;
const STATUS_STARTING: u64 = 1 << TAG_SHIFT;
const STATUS_READY: u64 = 2 << TAG_SHIFT;
const STATUS_STOPPING: u64 = 3 << TAG_SHIFT;
const STATUS_FAILED: u64 = 4 << TAG_SHIFT;
const STATUS_FINISHED: u64 = 5 << TAG_SHIFT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputWorkerStatus {
    Starting,
    Ready,
    Stopping,
    Failed(i32),
    Finished,
}

enum Command {
    Action(OutputAction),
    Refresh,
    Reset,
}

struct Shared {
    status: AtomicU64,
    stop: AtomicBool,
}

impl Shared {
    fn new() -> Self {
        Self {
            status: AtomicU64::new(STATUS_STARTING),
            stop: AtomicBool::new(false),
        }
    }

    fn status(&self) -> OutputWorkerStatus {
        decode_status(self.status.load(Ordering::Acquire))
    }

    fn fail(&self, error: i32) {
        self.stop.store(true, Ordering::Release);
        let failed = STATUS_FAILED | error as u32 as u64;
        loop {
            let current = self.status.load(Ordering::Acquire);
            match decode_status(current) {
                OutputWorkerStatus::Failed(_) | OutputWorkerStatus::Finished => return,
                _ => {
                    if self
                        .status
                        .compare_exchange(current, failed, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        return;
                    }
                }
            }
        }
    }

    fn fail_and_get_error(&self, error: i32) -> i32 {
        self.fail(error);
        match self.status() {
            OutputWorkerStatus::Failed(first_error) => first_error,
            _ => error,
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        loop {
            let current = self.status.load(Ordering::Acquire);
            match decode_status(current) {
                OutputWorkerStatus::Starting | OutputWorkerStatus::Ready => {
                    if self
                        .status
                        .compare_exchange(
                            current,
                            STATUS_STOPPING,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
                OutputWorkerStatus::Stopping
                | OutputWorkerStatus::Failed(_)
                | OutputWorkerStatus::Finished => return,
            }
        }
    }

    fn publish_ready(&self) -> bool {
        if self.stop.load(Ordering::Acquire) {
            return false;
        }
        self.status
            .compare_exchange(
                STATUS_STARTING,
                STATUS_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
            && !self.stop.load(Ordering::Acquire)
    }

    fn finish(&self) {
        loop {
            let current = self.status.load(Ordering::Acquire);
            match decode_status(current) {
                OutputWorkerStatus::Failed(_) | OutputWorkerStatus::Finished => return,
                _ => {
                    if self
                        .status
                        .compare_exchange(
                            current,
                            STATUS_FINISHED,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        return;
                    }
                }
            }
        }
    }
}

fn decode_status(value: u64) -> OutputWorkerStatus {
    match value & TAG_MASK {
        STATUS_STARTING => OutputWorkerStatus::Starting,
        STATUS_READY => OutputWorkerStatus::Ready,
        STATUS_STOPPING => OutputWorkerStatus::Stopping,
        STATUS_FAILED => OutputWorkerStatus::Failed(value as u32 as i32),
        STATUS_FINISHED => OutputWorkerStatus::Finished,
        _ => OutputWorkerStatus::Failed(-libc::EIO),
    }
}

pub(crate) struct OutputWorker {
    sender: Option<SyncSender<Command>>,
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl OutputWorker {
    pub(crate) fn spawn(gamepad: bool, desktop: bool) -> Result<Self, i32> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let shared = Arc::new(Shared::new());
        let worker_shared = Arc::clone(&shared);
        let join = thread::Builder::new()
            .name("wiiland-output".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    run_worker(gamepad, desktop, receiver, &worker_shared)
                }));
                match result {
                    Ok(Ok(())) => worker_shared.finish(),
                    Ok(Err(error)) => worker_shared.fail(error),
                    Err(_) => worker_shared.fail(-libc::EIO),
                }
            })
            .map_err(|error| io_errno(&error))?;
        Ok(Self {
            sender: Some(sender),
            shared,
            join: Some(join),
        })
    }

    pub(crate) fn status(&self) -> OutputWorkerStatus {
        self.shared.status()
    }

    pub(crate) fn try_action(&mut self, action: OutputAction) -> Result<(), i32> {
        self.try_send(Command::Action(action))
    }

    pub(crate) fn try_refresh(&mut self) -> Result<(), i32> {
        self.try_send(Command::Refresh)
    }

    pub(crate) fn try_reset(&mut self) -> Result<(), i32> {
        self.try_send(Command::Reset)
    }

    pub(crate) fn request_close(&mut self) -> Option<JoinHandle<()>> {
        self.shared.request_stop();
        drop(self.sender.take());
        self.join.take()
    }

    fn try_send(&mut self, command: Command) -> Result<(), i32> {
        match self.shared.status() {
            OutputWorkerStatus::Failed(error) => return Err(error),
            OutputWorkerStatus::Stopping | OutputWorkerStatus::Finished => {
                return Err(-libc::EPIPE);
            }
            OutputWorkerStatus::Starting | OutputWorkerStatus::Ready => {}
        }

        let Some(sender) = &self.sender else {
            return Err(-libc::EPIPE);
        };
        match sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(self.shared.fail_and_get_error(-libc::EAGAIN)),
            Err(TrySendError::Disconnected(_)) => Err(self.shared.fail_and_get_error(-libc::EPIPE)),
        }
    }
}

impl Drop for OutputWorker {
    fn drop(&mut self) {
        self.shared.request_stop();
        drop(self.sender.take());
    }
}

fn run_worker(
    gamepad: bool,
    desktop: bool,
    receiver: Receiver<Command>,
    shared: &Shared,
) -> Result<(), i32> {
    if shared.stop.load(Ordering::Acquire) {
        return Ok(());
    }

    let mut session =
        match WindowsOutputSession::connect_cancellable(gamepad, desktop, &shared.stop) {
            Ok(session) => session,
            Err(error)
                if shared.stop.load(Ordering::Acquire) && io_errno(&error) == -libc::ECANCELED =>
            {
                return Ok(());
            }
            Err(error) => return Err(io_errno(&error)),
        };
    if shared.stop.load(Ordering::Acquire) || !shared.publish_ready() {
        return session.close();
    }

    loop {
        if shared.stop.load(Ordering::Acquire) {
            return session.close();
        }
        let command = match receiver.recv() {
            Ok(command) => command,
            Err(_) => {
                shared.request_stop();
                return session.close();
            }
        };
        if shared.stop.load(Ordering::Acquire) {
            return session.close();
        }

        let result = match command {
            Command::Action(action) => session.process_action_cancellable(action, &shared.stop),
            Command::Refresh => session.refresh_cancellable(&shared.stop),
            Command::Reset => session.reset_cancellable(&shared.stop),
        };
        if let Err(error) = result {
            if shared.stop.load(Ordering::Acquire) && error == -libc::ECANCELED {
                return session.close();
            }
            return Err(error);
        }
    }
}

fn io_errno(error: &io::Error) -> i32 {
    -error.raw_os_error().unwrap_or(libc::EIO)
}
