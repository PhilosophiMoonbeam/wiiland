//! Configuration effects run together on a worker, outside the model and UI.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use std::time::Duration;

use tempfile::NamedTempFile;

use crate::model::{
    Completion, ConfigFailure, ConfigFailureStage, ConfigValue, Transaction, TransactionKind,
    parse_config_bytes,
};
use crate::process::{self, ProcessEvent, ProcessResult, ProcessTask};

const EVENT_QUEUE_CAPACITY: usize = 64;

pub enum ConfigEvent {
    Command { program: String, args: Vec<String> },
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    OmittedOutput(usize),
    Finished(Completion),
}

pub struct ConfigTask {
    events: Receiver<ConfigEvent>,
    cancelled: Arc<AtomicBool>,
}

impl ConfigTask {
    pub fn spawn(transaction: Transaction, program: String, default_path: Option<PathBuf>) -> Self {
        let (sender, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancelled);
        std::thread::spawn(move || {
            let mut omitted_output = 0;
            let result = run(
                &transaction,
                program,
                default_path,
                &sender,
                &stop,
                &mut omitted_output,
            );
            if omitted_output > 0 {
                let _ = sender.send(ConfigEvent::OmittedOutput(omitted_output));
            }
            let _ = sender.send(ConfigEvent::Finished(Completion::new(&transaction, result)));
        });
        Self { events, cancelled }
    }

    pub fn try_recv(&self) -> Result<ConfigEvent, TryRecvError> {
        self.events.try_recv()
    }
}

impl Drop for ConfigTask {
    fn drop(&mut self) {
        // The worker owns subprocess termination and reaping. Dropping this
        // receiver also releases a worker blocked on the bounded output queue.
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn failure(stage: ConfigFailureStage, error: impl std::fmt::Display) -> ConfigFailure {
    ConfigFailure {
        stage,
        message: error.to_string(),
    }
}

fn check_cancelled(stop: &AtomicBool) -> Result<(), ConfigFailure> {
    if stop.load(Ordering::Relaxed) {
        Err(failure(
            ConfigFailureStage::Cancelled,
            "Configuration operation cancelled",
        ))
    } else {
        Ok(())
    }
}

fn send(sender: &SyncSender<ConfigEvent>, event: ConfigEvent) -> Result<(), ConfigFailure> {
    sender.send(event).map_err(|_| {
        failure(
            ConfigFailureStage::Cancelled,
            "Configuration consumer closed",
        )
    })
}

fn forward_output(
    sender: &SyncSender<ConfigEvent>,
    event: ConfigEvent,
    bytes: usize,
    omitted_output: &mut usize,
) -> Result<(), ConfigFailure> {
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(mpsc::TrySendError::Full(_)) => {
            *omitted_output = omitted_output.saturating_add(bytes);
            Ok(())
        }
        Err(mpsc::TrySendError::Disconnected(_)) => Err(failure(
            ConfigFailureStage::Cancelled,
            "Configuration consumer closed",
        )),
    }
}

fn prepare(target: &Path, bytes: &[u8]) -> Result<NamedTempFile, ConfigFailure> {
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prepare = || -> std::io::Result<NamedTempFile> {
        std::fs::create_dir_all(parent)?;
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes)?;
        temporary.as_file().sync_all()?;
        Ok(temporary)
    };
    prepare().map_err(|error| failure(ConfigFailureStage::Preparation, error))
}

fn run(
    transaction: &Transaction,
    program: String,
    default_path: Option<PathBuf>,
    sender: &SyncSender<ConfigEvent>,
    stop: &AtomicBool,
    omitted_output: &mut usize,
) -> Result<ConfigValue, ConfigFailure> {
    check_cancelled(stop)?;
    let (temporary, args, stage) = match transaction.kind {
        TransactionKind::Load => (
            None,
            process::configured_args(
                &transaction.target,
                default_path.as_deref(),
                vec!["--dump-config".into()],
            ),
            ConfigFailureStage::Load,
        ),
        TransactionKind::Save => {
            let temporary = prepare(&transaction.target, &transaction.captured)?;
            let args = vec![
                "--check-config".into(),
                "--config".into(),
                temporary.path().to_string_lossy().into_owned(),
            ];
            (Some(temporary), args, ConfigFailureStage::Validation)
        }
    };
    check_cancelled(stop)?;
    send(
        sender,
        ConfigEvent::Command {
            program: program.clone(),
            args: args.clone(),
        },
    )?;
    let result = run_process(program, &args, sender, stop, stage, omitted_output)?;
    check_cancelled(stop)?;
    match temporary {
        Some(temporary) => {
            // Rename the same immutable snapshot that was validated. Neither a
            // later form edit nor a target change can alter this operation.
            temporary
                .persist(&transaction.target)
                .map_err(|error| failure(ConfigFailureStage::Persistence, error.error))?;
            Ok(ConfigValue::Saved)
        }
        None => parse_config_bytes(&result.stdout)
            .map(ConfigValue::Loaded)
            .map_err(|error| failure(ConfigFailureStage::Parsing, error)),
    }
}

fn run_process(
    program: String,
    args: &[String],
    sender: &SyncSender<ConfigEvent>,
    stop: &AtomicBool,
    stage: ConfigFailureStage,
    omitted_output: &mut usize,
) -> Result<ProcessResult, ConfigFailure> {
    let task = ProcessTask::spawn_capturing_stdout(program, args);
    loop {
        check_cancelled(stop)?;
        match task.recv_timeout(Duration::from_millis(20)) {
            Ok(ProcessEvent::Stdout(bytes)) => {
                let count = bytes.len();
                forward_output(sender, ConfigEvent::Stdout(bytes), count, omitted_output)?;
            }
            Ok(ProcessEvent::Stderr(bytes)) => {
                let count = bytes.len();
                forward_output(sender, ConfigEvent::Stderr(bytes), count, omitted_output)?;
            }
            Ok(ProcessEvent::Finished(result)) => {
                if result.success {
                    return Ok(result);
                }
                let message = result.error.unwrap_or_else(|| {
                    let stderr = String::from_utf8_lossy(&result.stderr);
                    format!(
                        "Daemon command failed (exit {:?}): {}",
                        result.code,
                        stderr.trim()
                    )
                });
                return Err(failure(stage, message));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(failure(
                    ConfigFailureStage::Worker,
                    "Configuration subprocess worker disconnected",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use super::*;
    use crate::model::{ApplyCompletion, ConfigModel};

    fn finish(task: &ConfigTask) -> Completion {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match task.try_recv() {
                Ok(ConfigEvent::Finished(completion)) => return completion,
                Ok(_) => {}
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("configuration worker did not finish: {error}"),
            }
        }
    }

    fn script(directory: &Path, contents: &str) -> String {
        let path = directory.join("daemon");
        fs::write(&path, format!("#!/bin/sh\n{contents}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn worker_persists_captured_save_without_ui_polling_and_retains_later_edits() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("first/config");
        let newer_target = directory.path().join("second/config");
        let mut model = ConfigModel::new(target.clone());
        model.config.pointer_speed = 17;
        model.mark_dirty();
        let captured = model.render();
        let transaction = model
            .begin(TransactionKind::Save, captured.clone())
            .unwrap();
        let task = ConfigTask::spawn(transaction, "/bin/true".into(), None);
        model.config.pointer_speed = 33;
        model.mark_dirty();
        model.set_path(newer_target.clone());

        // Do not consume even the initial command notification. Persistence
        // must complete independently of the application/frame loop.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !target.exists() {
            assert!(
                Instant::now() < deadline,
                "save needed UI polling to complete"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fs::read(&target).unwrap(), captured);
        assert!(!newer_target.exists());
        let completion = finish(&task);
        assert_eq!(completion.result, Ok(ConfigValue::Saved));
        assert_eq!(model.finish(&completion), ApplyCompletion::Stale);
        assert!(model.transaction.is_none());
        assert!(model.dirty);
        assert_eq!(model.config.pointer_speed, 33);
    }

    #[test]
    fn validation_failure_preserves_existing_file_and_releases_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config");
        fs::write(&target, b"original").unwrap();
        let mut model = ConfigModel::new(target.clone());
        model.mark_dirty();
        let transaction = model.begin(TransactionKind::Save, model.render()).unwrap();
        let program = script(
            directory.path(),
            "printf 'invalid configuration' >&2; exit 9",
        );
        let task = ConfigTask::spawn(transaction, program, None);
        let completion = finish(&task);
        let failure = completion.result.as_ref().unwrap_err();
        assert_eq!(failure.stage, ConfigFailureStage::Validation);
        assert!(failure.message.contains("invalid configuration"));
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(model.finish(&completion), ApplyCompletion::Failed);
        assert!(model.transaction.is_none());
        assert!(model.dirty);
    }

    #[test]
    fn noisy_validation_persists_with_stalled_ui_and_reports_omitted_log_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config");
        let mut model = ConfigModel::new(target.clone());
        let captured = model.render();
        let transaction = model
            .begin(TransactionKind::Save, captured.clone())
            .unwrap();
        let program = script(
            directory.path(),
            "dd if=/dev/zero bs=8192 count=96 2>/dev/null",
        );
        let task = ConfigTask::spawn(transaction, program, None);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !target.exists() {
            assert!(
                Instant::now() < deadline,
                "validation waited for UI log consumption"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fs::read(target).unwrap(), captured);
        let mut omitted = 0;
        let completion = loop {
            assert!(Instant::now() < deadline, "save completion not delivered");
            match task.try_recv() {
                Ok(ConfigEvent::OmittedOutput(bytes)) => omitted += bytes,
                Ok(ConfigEvent::Finished(completion)) => break completion,
                Ok(_) | Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
                Err(error) => panic!("save failed: {error}"),
            }
        };
        assert!(omitted > 0);
        assert_eq!(completion.result, Ok(ConfigValue::Saved));
    }

    #[test]
    fn preparation_and_persistence_errors_retain_their_stage_and_cause() {
        let directory = tempfile::tempdir().unwrap();
        let file_parent = directory.path().join("file");
        fs::write(&file_parent, b"original").unwrap();
        let directory_target = directory.path().join("directory");
        fs::create_dir(&directory_target).unwrap();
        for (target, stage) in [
            (file_parent.join("config"), ConfigFailureStage::Preparation),
            (directory_target.clone(), ConfigFailureStage::Persistence),
        ] {
            let mut model = ConfigModel::new(target);
            let transaction = model.begin(TransactionKind::Save, model.render()).unwrap();
            let task = ConfigTask::spawn(transaction, "/bin/true".into(), None);
            let completion = finish(&task);
            let failure = completion.result.as_ref().unwrap_err();
            assert_eq!(failure.stage, stage);
            assert!(!failure.message.is_empty());
            assert_eq!(model.finish(&completion), ApplyCompletion::Failed);
            assert!(model.transaction.is_none());
        }
        assert_eq!(fs::read(file_parent).unwrap(), b"original");
        assert!(directory_target.is_dir());
    }

    #[test]
    fn load_returns_parsed_values_and_stale_completion_preserves_form() {
        let directory = tempfile::tempdir().unwrap();
        let mut model = ConfigModel::new(directory.path().join("config"));
        let transaction = model.begin(TransactionKind::Load, Vec::new()).unwrap();
        let program = script(
            directory.path(),
            "printf 'profile=desktop\\npointer-speed=19\\n'",
        );
        let task = ConfigTask::spawn(transaction, program, None);
        model.config.pointer_speed = 55;
        model.mark_dirty();
        let completion = finish(&task);
        assert!(
            matches!(&completion.result, Ok(ConfigValue::Loaded(value)) if value.pointer_speed == 19)
        );
        assert_eq!(model.finish(&completion), ApplyCompletion::Stale);
        assert_eq!(model.config.pointer_speed, 55);
        assert!(model.dirty);
    }

    #[test]
    fn invalid_load_is_reported_at_parsing_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let mut model = ConfigModel::new(directory.path().join("config"));
        let transaction = model.begin(TransactionKind::Load, Vec::new()).unwrap();
        let program = script(directory.path(), "printf 'profile=invalid\\n'");
        let task = ConfigTask::spawn(transaction, program, None);
        let completion = finish(&task);
        assert_eq!(
            completion.result.as_ref().unwrap_err().stage,
            ConfigFailureStage::Parsing
        );
        assert_eq!(model.finish(&completion), ApplyCompletion::Failed);
    }

    #[test]
    fn dropping_save_cancels_validation_and_cleans_up_without_persisting() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config");
        fs::write(&target, b"original").unwrap();
        let mut model = ConfigModel::new(target.clone());
        let transaction = model.begin(TransactionKind::Save, model.render()).unwrap();
        let program = script(directory.path(), "printf '%s\\n' \"$$\"; exec sleep 30");
        let task = ConfigTask::spawn(transaction, program, None);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut stdout = String::new();
        let pid = loop {
            assert!(Instant::now() < deadline, "validation did not start");
            match task.try_recv() {
                Ok(ConfigEvent::Stdout(bytes)) => {
                    stdout.push_str(&String::from_utf8_lossy(&bytes));
                    if stdout.contains('\n') {
                        break stdout.trim().parse::<u32>().unwrap();
                    }
                }
                Ok(ConfigEvent::Finished(_)) => panic!("validation completed before cancellation"),
                Ok(_) | Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
                Err(error) => panic!("validation failed: {error}"),
            }
        };
        drop(task);
        while Path::new(&format!("/proc/{pid}")).exists()
            || fs::read_dir(directory.path()).unwrap().count() > 2
        {
            assert!(Instant::now() < deadline, "cancelled save did not clean up");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fs::read(target).unwrap(), b"original");
    }
}
