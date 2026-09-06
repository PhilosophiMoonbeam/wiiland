use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const EVENT_QUEUE_CAPACITY: usize = 64;
const READ_CHUNK_SIZE: usize = 8 * 1024;
const CAPTURE_LIMIT: usize = 1024 * 1024;
const PIPE_POLL_TIMEOUT_MS: i32 = 50;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Finished(ProcessResult),
}

#[derive(Debug, Default)]
struct ChildState {
    child: Option<Child>,
    terminate_requested: bool,
}

#[derive(Debug)]
struct SpawnGate {
    spawned: SyncSender<u32>,
    publish: Receiver<()>,
}

#[derive(Debug)]
pub struct ProcessTask {
    receiver: Option<Receiver<ProcessEvent>>,
    state: Arc<Mutex<ChildState>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessResult {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub error: Option<String>,
}

impl ProcessResult {
    pub fn unavailable(error: impl Into<String>) -> Self {
        Self {
            success: false,
            code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            error: Some(error.into()),
        }
    }
}

impl ProcessTask {
    pub fn spawn(program: impl Into<String>, args: &[String]) -> Self {
        Self::spawn_inner(program.into(), args.to_vec(), None, None)
    }

    pub fn spawn_capturing_stdout(program: impl Into<String>, args: &[String]) -> Self {
        Self::spawn_inner(program.into(), args.to_vec(), Some(CAPTURE_LIMIT), None)
    }

    fn spawn_inner(
        program: String,
        args: Vec<String>,
        stdout_capture_limit: Option<usize>,
        gate: Option<SpawnGate>,
    ) -> Self {
        let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let state = Arc::new(Mutex::new(ChildState::default()));
        let child_state = Arc::clone(&state);
        let worker = thread::spawn(move || {
            let spawned = Command::new(&program)
                .args(&args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn();
            let mut child = match spawned {
                Ok(child) => child,
                Err(error) => {
                    let _ = sender.send(ProcessEvent::Finished(ProcessResult::unavailable(
                        format!("{program}: {error}"),
                    )));
                    return;
                }
            };

            if let Some(gate) = gate {
                let _ = gate.spawned.send(child.id());
                let _ = gate.publish.recv();
            }

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            {
                let mut state = lock_state(&child_state);
                if state.terminate_requested {
                    let _ = child.kill();
                }
                state.child = Some(child);
            }

            let stdout_thread = stdout.map(|stream| {
                let sender = sender.clone();
                let state = Arc::clone(&child_state);
                thread::spawn(move || {
                    read_stream(
                        stream,
                        OutputStream::Stdout,
                        sender,
                        stdout_capture_limit,
                        &state,
                    )
                })
            });
            let stderr_thread = stderr.map(|stream| {
                let sender = sender.clone();
                let state = Arc::clone(&child_state);
                thread::spawn(move || {
                    read_stream(
                        stream,
                        OutputStream::Stderr,
                        sender,
                        stdout_capture_limit,
                        &state,
                    )
                })
            });

            let status = loop {
                let status = {
                    let mut state = lock_state(&child_state);
                    match state.child.as_mut().map(Child::try_wait) {
                        Some(Ok(Some(status))) => {
                            state.child.take();
                            Some(status)
                        }
                        Some(Err(error)) => {
                            let _ = state.child.take().and_then(|mut child| child.wait().ok());
                            let _ = sender.send(ProcessEvent::Finished(
                                ProcessResult::unavailable(format!("{program}: {error}")),
                            ));
                            return;
                        }
                        _ => None,
                    }
                };
                if let Some(status) = status {
                    break status;
                }
                thread::sleep(Duration::from_millis(10));
            };

            let stdout = stdout_thread
                .and_then(|reader| reader.join().ok())
                .unwrap_or_default();
            let stderr = stderr_thread
                .and_then(|reader| reader.join().ok())
                .unwrap_or_default();
            let error = [stdout.error, stderr.error]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
            let error = (!error.is_empty()).then_some(error);
            let _ = sender.send(ProcessEvent::Finished(ProcessResult {
                success: status.success() && error.is_none(),
                code: status.code(),
                stdout: stdout.captured,
                stderr: stderr.captured,
                error,
            }));
        });
        Self {
            receiver: Some(receiver),
            state,
            worker: Some(worker),
        }
    }

    pub fn try_recv(&self) -> Result<Option<ProcessEvent>, TryRecvError> {
        match self
            .receiver
            .as_ref()
            .expect("process receiver is present until drop")
            .try_recv()
        {
            Ok(value) => Ok(Some(value)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<ProcessEvent, mpsc::RecvTimeoutError> {
        self.receiver
            .as_ref()
            .expect("process receiver is present until drop")
            .recv_timeout(timeout)
    }

    /// Request termination without blocking the UI thread. The worker owns the
    /// corresponding wait and reports the resulting exit status normally.
    pub fn terminate(&self) {
        request_termination(&self.state);
    }

    #[cfg(test)]
    fn spawn_paused_before_publication(
        program: impl Into<String>,
        args: &[String],
    ) -> (Self, Receiver<u32>, SyncSender<()>) {
        let (spawned_sender, spawned_receiver) = mpsc::sync_channel(1);
        let (publish_sender, publish_receiver) = mpsc::sync_channel(1);
        let task = Self::spawn_inner(
            program.into(),
            args.to_vec(),
            None,
            Some(SpawnGate {
                spawned: spawned_sender,
                publish: publish_receiver,
            }),
        );
        (task, spawned_receiver, publish_sender)
    }
}

impl Drop for ProcessTask {
    fn drop(&mut self) {
        request_termination(&self.state);
        // Closing the bounded event queue releases reader threads even if the UI
        // stopped draining output before the task was dropped.
        self.receiver.take();
        // The worker owns and reaps the child before joining its pipe readers.
        // Detach cleanup: descendants may retain those pipes, and neither their
        // lifetime nor a pending spawn may block the application's event loop.
        drop(self.worker.take());
    }
}

#[derive(Debug, Default)]
struct ReaderResult {
    captured: Vec<u8>,
    error: Option<String>,
}

fn read_stream(
    mut stream: impl Read + AsFd,
    output_stream: OutputStream,
    sender: SyncSender<ProcessEvent>,
    capture_limit: Option<usize>,
    state: &Mutex<ChildState>,
) -> ReaderResult {
    let mut result = ReaderResult::default();
    let mut buffer = [0_u8; READ_CHUNK_SIZE];
    loop {
        if lock_state(state).terminate_requested {
            break;
        }
        // A descendant can inherit stdout/stderr after our direct child exits.
        // Poll before reading so cancellation does not depend on that descendant
        // writing again or closing its descriptors. This is the only reader.
        let mut descriptor = libc::pollfd {
            fd: stream.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: descriptor points to one initialized pollfd and its borrowed
        // descriptor remains owned by stream throughout this call.
        let ready = unsafe { libc::poll(&mut descriptor, 1, PIPE_POLL_TIMEOUT_MS) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            result.error = Some(format!("failed to poll {output_stream:?}: {error}"));
            break;
        }
        if ready == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            result.error = Some(format!("invalid {output_stream:?} descriptor"));
            break;
        }
        if lock_state(state).terminate_requested {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => {
                if let Some(limit) = capture_limit {
                    let remaining = limit.saturating_sub(result.captured.len());
                    result
                        .captured
                        .extend_from_slice(&buffer[..length.min(remaining)]);
                    if length > remaining && result.error.is_none() {
                        result.error = Some(format!(
                            "{output_stream:?} exceeded the {CAPTURE_LIMIT}-byte capture limit"
                        ));
                    }
                }
                let event = match output_stream {
                    OutputStream::Stdout => ProcessEvent::Stdout(buffer[..length].to_vec()),
                    OutputStream::Stderr => ProcessEvent::Stderr(buffer[..length].to_vec()),
                };
                if sender.send(event).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                result.error = Some(format!("failed to read {output_stream:?}: {error}"));
                break;
            }
        }
    }
    result
}

fn lock_state(state: &Mutex<ChildState>) -> MutexGuard<'_, ChildState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn request_termination(state: &Mutex<ChildState>) {
    let mut state = lock_state(state);
    state.terminate_requested = true;
    if let Some(child) = state.child.as_mut() {
        let _ = child.kill();
    }
}

pub fn service_args(action: &str) -> Vec<String> {
    vec![
        "--user".to_owned(),
        action.to_owned(),
        "wiilandd.service".to_owned(),
    ]
}

pub fn configured_args(
    model_path: &std::path::Path,
    default_path: Option<&std::path::Path>,
    mut args: Vec<String>,
) -> Vec<String> {
    let normalize = |path: &std::path::Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    };
    let explicit = default_path
        .map(|default| normalize(model_path) != normalize(default))
        .unwrap_or(true);
    if explicit {
        args = [
            vec![
                "--config".to_owned(),
                model_path.to_string_lossy().into_owned(),
            ],
            args,
        ]
        .concat();
    }
    args
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;

    fn shell(script: &str) -> ProcessTask {
        ProcessTask::spawn("/bin/sh", &["-c".to_owned(), script.to_owned()])
    }

    #[test]
    fn cancellation_releases_reader_while_an_inherited_pipe_stays_open() {
        let mut descriptors = [-1; 2];
        // SAFETY: descriptors has space for the two newly owned pipe handles.
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        // SAFETY: pipe returned two distinct, valid descriptors, each transferred
        // exactly once into a File. The writer represents an inheriting process.
        let reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
        let mut inherited_writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
        let state = Arc::new(Mutex::new(ChildState::default()));
        let reader_state = Arc::clone(&state);
        let (output, events) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let result = read_stream(reader, OutputStream::Stdout, output, None, &reader_state);
            done.send(result.error).unwrap();
        });
        inherited_writer.write_all(b"started").unwrap();
        assert_eq!(
            events.recv_timeout(Duration::from_secs(2)).unwrap(),
            ProcessEvent::Stdout(b"started".to_vec())
        );
        request_termination(&state);
        assert_eq!(
            finished
                .recv_timeout(Duration::from_secs(2))
                .expect("reader waited for inherited pipe EOF"),
            None
        );
        worker.join().unwrap();
        // Keep the inherited writer alive until after cleanup is confirmed.
        drop(inherited_writer);
    }

    #[test]
    fn streams_output_before_process_exit() {
        let task = shell("printf early; sleep 0.2; printf late");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut streamed = Vec::new();
        let mut early = false;
        let mut finished = false;
        while Instant::now() < deadline && !early {
            match task.try_recv() {
                Ok(Some(ProcessEvent::Stdout(bytes) | ProcessEvent::Stderr(bytes))) => {
                    streamed.extend_from_slice(&bytes);
                    early = streamed.windows(5).any(|part| part == b"early");
                }
                Ok(Some(ProcessEvent::Finished(_))) => finished = true,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("event channel failed: {error}"),
            }
        }
        assert!(early, "early output was not delivered incrementally");
        assert!(
            !finished,
            "process finished before early output was observed"
        );
    }

    #[test]
    fn capturing_stdout_preserves_finite_completion_data() {
        let task = ProcessTask::spawn_capturing_stdout(
            "/bin/sh",
            &["-c".to_owned(), "printf calibration-data".to_owned()],
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = loop {
            assert!(Instant::now() < deadline, "capture process did not finish");
            match task.try_recv() {
                Ok(Some(ProcessEvent::Finished(result))) => break result,
                Ok(Some(ProcessEvent::Stdout(_) | ProcessEvent::Stderr(_))) | Ok(None) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("event channel failed: {error}"),
            }
        };
        assert!(result.success);
        assert_eq!(result.stdout.as_slice(), b"calibration-data");
    }

    #[test]
    fn capturing_stdout_is_finite_and_reports_overflow() {
        let task = ProcessTask::spawn_capturing_stdout(
            "/bin/sh",
            &[
                "-c".to_owned(),
                format!("yes x | dd bs=1 count={} 2>/dev/null", CAPTURE_LIMIT + 1),
            ],
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let result = loop {
            assert!(Instant::now() < deadline, "capture process did not finish");
            match task.try_recv() {
                Ok(Some(ProcessEvent::Finished(result))) => break result,
                Ok(Some(ProcessEvent::Stdout(_) | ProcessEvent::Stderr(_))) | Ok(None) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("event channel failed: {error}"),
            }
        };
        assert_eq!(result.stdout.len(), CAPTURE_LIMIT);
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("capture limit"))
        );
    }

    #[test]
    fn normal_completion_is_reaped_before_result_delivery() {
        let (task, spawned, publish) = ProcessTask::spawn_paused_before_publication(
            "/bin/sh",
            &["-c".to_owned(), "exit 0".to_owned()],
        );
        let pid = spawned
            .recv_timeout(Duration::from_secs(2))
            .expect("child pid");
        publish.send(()).expect("release publication gate");
        let deadline = Instant::now() + Duration::from_secs(2);
        let result = loop {
            assert!(Instant::now() < deadline, "process did not finish");
            match task.try_recv() {
                Ok(Some(ProcessEvent::Finished(result))) => break result,
                Ok(Some(ProcessEvent::Stdout(_) | ProcessEvent::Stderr(_))) | Ok(None) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("event channel failed: {error}"),
            }
        };
        assert!(result.success);
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "normal completion was not reaped before delivery"
        );
    }

    #[test]
    fn drop_returns_before_publication_and_worker_eventually_reaps_child() {
        let (task, spawned, publish) = ProcessTask::spawn_paused_before_publication(
            "/bin/sh",
            &["-c".to_owned(), "exec sleep 30".to_owned()],
        );
        let pid = spawned
            .recv_timeout(Duration::from_secs(2))
            .expect("child pid");
        let (dropped, finished_drop) = mpsc::sync_channel(1);
        let dropping = thread::spawn(move || {
            drop(task);
            dropped.send(()).unwrap();
        });
        // The worker cannot publish or reap yet. Drop must still return.
        finished_drop
            .recv_timeout(Duration::from_secs(2))
            .expect("drop blocked on worker cleanup");
        publish.send(()).expect("release publication gate");
        dropping.join().expect("task drop");
        let deadline = Instant::now() + Duration::from_secs(2);
        while Path::new(&format!("/proc/{pid}")).exists() {
            assert!(
                Instant::now() < deadline,
                "child remained alive or unreaped after cancellation"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}
