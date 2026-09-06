use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use wiiland_core::Profile;

use crate::config_task::{ConfigEvent, ConfigTask};
use crate::live::{CalibrationResult, CaptureResult, QueryResult};
use crate::model::{self, ApplyCompletion, CalibrationTransaction, ConfigModel, TransactionKind};
use crate::process::{self, ProcessEvent, ProcessResult, ProcessTask};
use crate::theme;

#[path = "editors.rs"]
mod editors;
use editors::{draw_aim, draw_bindings, draw_profile, draw_rules, field_row};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Tab {
    Overview,
    Configuration,
    Validation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum ConfigSection {
    Pointer,
    Motion,
    Bindings,
    Rules,
}

impl ConfigSection {
    const ALL: [Self; 4] = [Self::Pointer, Self::Motion, Self::Bindings, Self::Rules];
    fn label(self) -> &'static str {
        match self {
            Self::Pointer => "Profile & pointer",
            Self::Motion => "Motion aiming",
            Self::Bindings => "Button bindings",
            Self::Rules => "Device rules",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationKind {
    Trace,
    Calibration,
}

struct PendingConfig {
    transaction: model::Transaction,
    worker: ConfigTask,
    restart_after_save: bool,
}
struct ValidationTask {
    kind: ValidationKind,
    cancel_requested: bool,
    process: CaptureTask,
    calibration: Option<CalibrationOwnership>,
}

enum CaptureTask {
    Direct(ProcessTask),
    Daemon(Box<crate::live::Capture>),
}
impl CaptureTask {
    fn terminate(&self) {
        match self {
            Self::Direct(task) => task.terminate(),
            Self::Daemon(task) => task.cancel(),
        }
    }
    fn poll(
        &mut self,
        model: &mut ConfigModel,
        kind: ValidationKind,
    ) -> Option<Result<CaptureResult, String>> {
        match self {
            Self::Direct(task) => {
                poll_process(task, model).map(|result| decode_capture_result(result, kind))
            }
            Self::Daemon(task) => task.poll(model),
        }
    }
}

struct CalibrationOwnership {
    transaction: CalibrationTransaction,
    device: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceAction {
    Restart,
}

impl ServiceAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "restart",
        }
    }
}

#[derive(Default)]
struct PendingServiceActions {
    pending: Option<ServiceAction>,
}

impl PendingServiceActions {
    fn request_restart(&mut self, service_active: bool) -> Option<ServiceAction> {
        if service_active {
            self.pending = Some(ServiceAction::Restart);
            None
        } else {
            self.pending = None;
            Some(ServiceAction::Restart)
        }
    }

    fn after_completion(&mut self) -> Option<ServiceAction> {
        self.pending.take()
    }
}

pub struct ControlCenter {
    pub model: ConfigModel,
    config_task: Option<PendingConfig>,
    command_task: Option<(String, ProcessTask)>,
    service_task: Option<(String, ProcessTask)>,
    pending_service_actions: PendingServiceActions,
    service_program: &'static str,
    validation_task: Option<ValidationTask>,
    tab: Tab,
    config_section: ConfigSection,
    emblem: Option<egui::TextureHandle>,
    output_open: bool,
    reload_confirmation: bool,
    close_approved: bool,
    service_status: String,
    daemon_status: String,
    status: String,
    close_confirmation: bool,
    trace_device: String,
    trace_filter: String,
    trace_profile: Option<Profile>,
    direct_capture: bool,
    ipc_query: Option<crate::live::Query>,
}

impl ControlCenter {
    pub fn new(model: ConfigModel) -> Self {
        Self {
            model,
            config_task: None,
            command_task: None,
            service_task: None,
            pending_service_actions: PendingServiceActions::default(),
            service_program: "systemctl",
            validation_task: None,
            tab: Tab::Overview,
            config_section: ConfigSection::Pointer,
            emblem: None,
            output_open: false,
            reload_confirmation: false,
            close_approved: false,
            service_status: "Checking…".to_owned(),
            daemon_status: "Daemon status pending".to_owned(),
            status: "Ready".to_owned(),
            close_confirmation: false,
            trace_device: String::new(),
            trace_filter: "all".to_owned(),
            trace_profile: None,
            direct_capture: false,
            ipc_query: None,
        }
    }

    pub fn initialize(model: ConfigModel) -> Self {
        let mut application = Self::new(model);
        application.begin_load(false);
        application.service_action("is-active");
        application.ipc_query = Some(crate::live::Query::start(false));
        application
    }

    pub fn backend_name() -> &'static str {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "Wayland"
        } else if std::env::var_os("DISPLAY").is_some() {
            "X11"
        } else {
            "unknown"
        }
    }

    pub fn begin_load(&mut self, _report_errors: bool) {
        if self.config_task.is_some() {
            return;
        }
        let Some(transaction) = self.model.begin(TransactionKind::Load, Vec::new()) else {
            return;
        };
        self.spawn_config_task(transaction, false);
        self.status = "Loading effective configuration".to_owned();
    }

    fn begin_save(&mut self, restart: bool) {
        if self.config_task.is_some() || self.model.validate_form().is_err() {
            self.status = "Configuration has validation errors".to_owned();
            return;
        }
        if self.model.config_path.as_os_str().is_empty() {
            self.status = "Choose a configuration target".to_owned();
            return;
        }
        let bytes = self.model.render();
        let Some(transaction) = self.model.begin(TransactionKind::Save, bytes) else {
            return;
        };
        self.spawn_config_task(transaction, restart);
        self.status = if restart {
            "Validating configuration before save (restart requested)".to_owned()
        } else {
            "Validating configuration before save".to_owned()
        };
    }

    fn spawn_config_task(&mut self, transaction: model::Transaction, restart: bool) {
        let worker = ConfigTask::spawn(
            transaction.clone(),
            self.model.daemon_program().to_owned(),
            ConfigModel::default_path(),
        );
        self.config_task = Some(PendingConfig {
            transaction,
            worker,
            restart_after_save: restart,
        });
    }

    fn poll_config_task(&mut self) {
        let Some(task) = self.config_task.as_ref() else {
            return;
        };
        let mut completion = None;
        for _ in 0..64 {
            match task.worker.try_recv() {
                Ok(ConfigEvent::Command { program, args }) => self.model
                    .append_output(&format!("$ {} {}\n", program, shell_args(&args))),
                Ok(ConfigEvent::Stdout(bytes) | ConfigEvent::Stderr(bytes)) => self.model
                    .append_output(&String::from_utf8_lossy(&bytes)),
                Ok(ConfigEvent::OmittedOutput(bytes)) => self.model.append_output(&format!(
                    "\nConfiguration log omitted {bytes} bytes while the UI was busy; validation results were retained.\n"
                )),
                Ok(ConfigEvent::Finished(result)) => {
                    completion = Some(result);
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    completion = Some(model::Completion::new(&task.transaction, Err(model::ConfigFailure {
                        stage: model::ConfigFailureStage::Worker,
                        message: "Configuration worker disconnected".to_owned(),
                    })));
                    break;
                }
            }
        }
        let Some(completion) = completion else {
            return;
        };
        let task = self.config_task.take().expect("task exists");
        let restart = task.restart_after_save
            && task.transaction.kind == TransactionKind::Save
            && !self.model.is_explicit_target(&task.transaction.target);
        if let Err(error) = &completion.result {
            self.model
                .append_output(&format!("Configuration error: {error}\n"));
        }
        let outcome = self.model.finish(&completion);
        match outcome {
            ApplyCompletion::Applied => {
                self.status = match task.transaction.kind {
                    TransactionKind::Load => "Configuration loaded".to_owned(),
                    TransactionKind::Save => "Configuration saved".to_owned(),
                }
            }
            ApplyCompletion::Stale => {
                self.status = match task.transaction.kind {
                    TransactionKind::Load => {
                        "Load discarded because the form or target changed".to_owned()
                    }
                    TransactionKind::Save => {
                        "Captured configuration saved; newer edits or target remain unchanged"
                            .to_owned()
                    }
                }
            }
            ApplyCompletion::Failed => {
                self.output_open = true;
                self.status = match &completion.result {
                    Err(error) => format!("Configuration operation failed: {error}"),
                    Ok(_) => "Configuration operation returned an unexpected result".to_owned(),
                };
            }
        }
        if restart && outcome == ApplyCompletion::Applied {
            self.request_restart_after_save();
        }
    }

    fn append_result_error(&mut self, result: &ProcessResult) {
        if let Some(error) = &result.error {
            self.model
                .append_output(&format!("process error: {error}\n"));
        }
    }

    fn run_command(&mut self, args: Vec<String>, config_sensitive: bool) {
        if args.first().is_some_and(|arg| arg == "--list") {
            if self.ipc_query.is_none() {
                self.ipc_query = Some(crate::live::Query::start(true));
            }
            self.output_open = true;
            return;
        }
        if self.command_task.is_some() {
            return;
        }
        let args = if config_sensitive {
            process::configured_args(
                &self.model.config_path,
                ConfigModel::default_path().as_deref(),
                args,
            )
        } else {
            args
        };
        let command = self.model.daemon_program().to_owned();
        self.model
            .append_output(&format!("$ {} {}\n", command, shell_args(&args)));
        self.command_task = Some((command.clone(), ProcessTask::spawn(command, &args)));
        self.output_open = true;
        self.status = "Command running".to_owned();
    }

    fn poll_command(&mut self) {
        let Some((_, task)) = self.command_task.as_ref() else {
            return;
        };
        let result = match poll_process(task, &mut self.model) {
            Some(result) => result,
            None => return,
        };
        self.command_task = None;
        self.append_result_error(&result);
        self.status = if result.success {
            "Command succeeded".to_owned()
        } else {
            format!(
                "Command failed (exit {})",
                result
                    .code
                    .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
            )
        };
    }

    fn service_action(&mut self, action: &str) {
        if self.service_task.is_some() {
            return;
        }
        self.dispatch_service_action(action);
    }

    fn request_restart_after_save(&mut self) {
        let active = self.service_task.is_some();
        if let Some(action) = self.pending_service_actions.request_restart(active) {
            self.dispatch_service_action(action.as_str());
        } else {
            self.service_status = "Restart queued…".to_owned();
        }
    }

    fn dispatch_service_action(&mut self, action: &str) {
        let args = process::service_args(action);
        self.model.append_output(&format!(
            "$ {} {}\n",
            self.service_program,
            shell_args(&args)
        ));
        self.service_status = format!("{}…", capitalize(action));
        self.service_task = Some((
            action.to_owned(),
            if action == "is-active" {
                ProcessTask::spawn_capturing_stdout(self.service_program, &args)
            } else {
                ProcessTask::spawn(self.service_program, &args)
            },
        ));
    }

    fn poll_service(&mut self) {
        let Some((action, task)) = self.service_task.as_ref() else {
            return;
        };
        let result = match poll_process(task, &mut self.model) {
            Some(result) => result,
            None => return,
        };
        let action = action.clone();
        self.service_task = None;
        self.append_result_error(&result);
        if action == "is-active" {
            self.service_status = service_query_status(&result).to_owned();
        } else if result.success {
            self.status = format!("Service {action} succeeded");
        } else {
            self.service_status = "Unavailable".to_owned();
            self.status = format!("Service {action} failed · see activity log");
            self.output_open = true;
        }
        if let Some(next) = self.pending_service_actions.after_completion() {
            self.dispatch_service_action(next.as_str());
        } else if action != "is-active" && result.success {
            self.dispatch_service_action("is-active");
        }
    }

    fn start_trace(&mut self) {
        if self.validation_task.is_some() {
            self.status = "Another live validation task is already running".to_owned();
            return;
        }
        let mut args = vec![
            "--dry-run".to_owned(),
            "--no-ipc".to_owned(),
            format!("--trace-events={}", self.trace_filter),
            "--verbose".to_owned(),
        ];
        if let Some(profile) = self.trace_profile {
            args.extend([
                "--profile".to_owned(),
                profile.as_str().unwrap_or("gamepad").to_owned(),
            ]);
        }
        if !self.trace_device.trim().is_empty() {
            args.extend(["--device".to_owned(), self.trace_device.trim().to_owned()]);
        }
        args = process::configured_args(
            &self.model.config_path,
            ConfigModel::default_path().as_deref(),
            args,
        );
        let command = self.model.daemon_program().to_owned();
        if self.direct_capture {
            self.model
                .append_output(&format!("$ {} {}\n", command, shell_args(&args)));
        }
        self.validation_task = Some(ValidationTask {
            kind: ValidationKind::Trace,
            cancel_requested: false,
            process: if self.direct_capture {
                CaptureTask::Direct(ProcessTask::spawn(command, &args))
            } else {
                CaptureTask::Daemon(Box::new(crate::live::Capture::start(
                    self.trace_device.trim().to_owned(),
                    self.trace_filter
                        .parse()
                        .unwrap_or(wiiland_core::TraceFilter::All),
                    None,
                )))
            },
            calibration: None,
        });
        self.output_open = true;
        self.status = "Trace running".to_owned();
    }

    fn start_calibration(&mut self) {
        if self.validation_task.is_some() {
            self.status = "Wait for the current trace or calibration capture to finish".to_owned();
            return;
        }
        let device = self.trace_device.trim().to_owned();
        let Some(transaction) = self.model.begin_calibration() else {
            self.status = "Another calibration transaction is already running".to_owned();
            return;
        };
        let mut args = vec![
            "--calibrate-aim".to_owned(),
            "--aim-calibration-duration".to_owned(),
            transaction.captured.aim_calibration_duration.to_string(),
        ];
        if !device.is_empty() {
            args.extend(["--device".to_owned(), device.clone()]);
        }
        args = process::configured_args(
            &transaction.target,
            ConfigModel::default_path().as_deref(),
            args,
        );
        let command = transaction.daemon_program.clone();
        if self.direct_capture {
            self.model
                .append_output(&format!("$ {} {}\n", command, shell_args(&args)));
        }
        self.validation_task = Some(ValidationTask {
            kind: ValidationKind::Calibration,
            cancel_requested: false,
            process: if self.direct_capture {
                CaptureTask::Direct(ProcessTask::spawn_capturing_stdout(command, &args))
            } else {
                CaptureTask::Daemon(Box::new(crate::live::Capture::start(
                    device.clone(),
                    wiiland_core::TraceFilter::All,
                    Some(Duration::from_secs(
                        transaction.captured.aim_calibration_duration as u64,
                    )),
                )))
            },
            calibration: Some(CalibrationOwnership {
                transaction,
                device,
            }),
        });
        self.output_open = true;
        self.status = "Calibration running".to_owned();
    }

    fn stop_capture(&mut self) {
        if let Some(task) = &mut self.validation_task {
            task.cancel_requested = true;
            task.process.terminate();
            self.status = "Stopping capture…".to_owned();
        }
    }

    fn poll_validation(&mut self) {
        let Some(task) = self.validation_task.as_mut() else {
            return;
        };
        let result = match task.process.poll(&mut self.model, task.kind) {
            Some(result) => result,
            None => return,
        };
        let task = self.validation_task.take().expect("task exists");
        if task.cancel_requested {
            if let Some(ownership) = task.calibration {
                self.model.finish_calibration(&ownership.transaction);
            }
            self.status = "Capture stopped".to_owned();
            return;
        }
        if let Err(error) = &result {
            self.model.append_output(&format!("{error}\n"));
        }
        match task.kind {
            ValidationKind::Trace => {
                self.status = if result.is_ok() {
                    "Trace stopped"
                } else {
                    "Trace failed"
                }
                .to_owned();
            }
            ValidationKind::Calibration => {
                let ownership = task
                    .calibration
                    .expect("calibration task carries captured ownership");
                self.complete_calibration(
                    ownership,
                    result.and_then(|result| match result {
                        CaptureResult::Calibrated(value) => Ok(value),
                        CaptureResult::TraceStopped => {
                            Err("Capture returned no calibration".into())
                        }
                    }),
                );
            }
        }
    }

    fn complete_calibration(
        &mut self,
        ownership: CalibrationOwnership,
        result: Result<CalibrationResult, String>,
    ) {
        let model_owned = self.model.finish_calibration(&ownership.transaction);
        let device_owned = self.trace_device.trim() == ownership.device.as_str();
        match result {
            Err(_) => self.status = "Calibration failed".to_owned(),
            Ok(_) if !model_owned || !device_owned => {
                self.status =
                    "Calibration discarded because the form, target, or capture changed".to_owned();
            }
            Ok(value) => {
                if let Some(accel) = value.accel {
                    self.model.config.aim_accel_zero = Some(accel);
                }
                if let Some(motion) = value.motion_plus {
                    self.model.config.aim_motion_plus_bias = Some(motion);
                }
                self.model.mark_dirty();
                self.status = "Calibration values applied; save to persist them".to_owned();
            }
        }
    }

    fn request_reload(&mut self) {
        if self.model.dirty {
            self.reload_confirmation = true;
        } else {
            self.begin_load(true);
        }
    }

    fn draw_overview(&mut self, ui: &mut egui::Ui) {
        theme::heading(
            ui,
            "Your controller workspace.",
            "Connect to WiiLand, shape your controls, then check them in motion.",
        );
        ui.horizontal_wrapped(|ui| {
            for (label, tab) in [
                ("01  Connect", Tab::Overview),
                ("02  Configure", Tab::Configuration),
                ("03  Test & calibrate", Tab::Validation),
            ] {
                if ui
                    .add(egui::Button::selectable(self.tab == tab, label))
                    .clicked()
                {
                    self.tab = tab;
                }
            }
        });
        ui.add_space(12.0);
        if ui.available_width() >= 620.0 {
            ui.columns(2, |columns| {
                columns[0].with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    self.draw_service_card(ui);
                });
                columns[1].with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    self.draw_discovery_card(ui);
                });
            });
        } else {
            self.draw_service_card(ui);
            ui.add_space(8.0);
            self.draw_discovery_card(ui);
        }
        ui.add_space(12.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.collapsing("Configuration file & advanced settings", |ui| {
                theme::note(ui, if self.model.is_explicit_path() {
                    "Custom file: saving exports these settings. The background service still uses its default configuration."
                } else {
                    "Your file layers over system defaults. Save and restart to apply changes to the service."
                });
                let label = ui.label("Configuration file");
                let mut path = self.model.config_path.to_string_lossy().into_owned();
                if ui.add(egui::TextEdit::singleline(&mut path)
                    .desired_width(f32::INFINITY)
                    .min_size(egui::vec2(0.0, 30.0)))
                    .labelled_by(label.id).changed()
                {
                    self.model.set_path(PathBuf::from(path));
                }
                ui.add(egui::Label::new(egui::RichText::new(self.model.config_path.to_string_lossy()).small()).wrap());
                let label = ui.label("Daemon executable");
                if ui.add(egui::TextEdit::singleline(&mut self.model.daemon_path)
                    .desired_width(f32::INFINITY)
                    .min_size(egui::vec2(0.0, 30.0)))
                    .labelled_by(label.id).changed()
                {
                    self.model.revision = self.model.revision.wrapping_add(1);
                }
                theme::note(ui, &format!("Window system: {}", Self::backend_name()));
            });
        });
    }

    fn draw_service_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Background service");
            ui.horizontal_wrapped(|ui| {
                if self.service_task.is_some() {
                    ui.spinner();
                }
                theme::badge(
                    ui,
                    &self.service_status,
                    matches!(self.service_status.as_str(), "Unavailable" | "Failed" | "Stopped"),
                );
            });
            theme::note(ui, match self.service_status.as_str() {
                "Unavailable" => "Cannot reach the user service. Check readiness for installation and permission details.",
                "Stopped" => "Start the service to connect controllers and capture live input.",
                "Failed" => "The service failed. Check readiness, then try starting it again.",
                "Running" => "Saved controls are handled in the background.",
                _ => "Checking the user service. Refresh to request its current state.",
            });
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let running = self.service_status == "Running";
                let (label, action) = if running { ("Restart", "restart") } else { ("Start", "start") };
                if theme::primary(ui, label, self.service_task.is_none()).clicked() {
                    self.service_action(action);
                }
                if ui.add_enabled(self.service_task.is_none(), egui::Button::new("Refresh")).clicked() {
                    self.service_action("is-active");
                }
                ui.menu_button("Service actions", |ui| {
                    for (label, action) in [("Start", "start"), ("Stop", "stop"), ("Restart", "restart")] {
                        if ui.add_enabled(self.service_task.is_none(), egui::Button::new(label)).clicked() {
                            self.service_action(action);
                            ui.close();
                        }
                    }
                });
            });
            ui.add_space(4.0);
            ui.add(egui::Label::new(egui::RichText::new(&self.daemon_status)
                .small().color(theme::Palette::for_ui(ui).muted)).wrap());
            ui.horizontal(|ui| {
                if ui.add_enabled(self.ipc_query.is_none(), egui::Button::new("Refresh daemon status")).clicked() {
                    self.ipc_query = Some(crate::live::Query::start(false));
                    self.output_open = true;
                }
            });
        });
    }

    fn draw_discovery_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Connect a controller");
            theme::note(ui, "Pair your Wii controller in Linux Bluetooth settings, then find it through the running daemon.");
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if theme::primary(ui, "Find devices", self.ipc_query.is_none()).clicked() {
                    self.run_command(vec!["--list".to_owned(), "--verbose".to_owned()], false);
                }
                if ui.add_enabled(self.command_task.is_none(), egui::Button::new("Check readiness")).clicked() {
                    self.run_command(vec!["--doctor".to_owned()], true);
                }
            });
            theme::note(ui, "Device details and readiness results open in the activity log.");
            ui.add_space(6.0);
            ui.separator();
            theme::note(ui, "Already connected?");
            if ui.button("Set up your controls").clicked() {
                self.tab = Tab::Configuration;
            }
        });
    }

    fn draw_configuration(&mut self, ui: &mut egui::Ui) {
        theme::heading(
            ui,
            "Configure your controls.",
            "Choose how the controller behaves. Changes stay local until you save.",
        );
        ui.horizontal_wrapped(|ui| {
            for section in ConfigSection::ALL {
                if ui
                    .add(egui::Button::selectable(
                        self.config_section == section,
                        section.label(),
                    ))
                    .clicked()
                {
                    self.config_section = section;
                }
            }
        });
        ui.add_space(10.0);
        match self.config_section {
            ConfigSection::Pointer => draw_profile(ui, &mut self.model),
            ConfigSection::Motion => draw_aim(ui, &mut self.model),
            ConfigSection::Bindings => draw_bindings(ui, &mut self.model),
            ConfigSection::Rules => draw_rules(ui, &mut self.model),
        }
    }

    fn draw_save_bar(&mut self, ui: &mut egui::Ui) {
        let validation = self.model.validate_form();
        let busy = self.config_task.is_some();
        let valid = validation.is_ok() && !self.model.config_path.as_os_str().is_empty();
        let custom = self.model.is_explicit_path();
        ui.horizontal_wrapped(|ui| {
            theme::badge(
                ui,
                if self.model.dirty {
                    "Unsaved changes"
                } else {
                    "No pending edits"
                },
                self.model.dirty,
            );
            if busy {
                ui.spinner();
            }
            if ui
                .add_enabled(!busy, egui::Button::new("Reload"))
                .on_hover_text("Reload from disk. Unsaved changes require confirmation.")
                .clicked()
            {
                self.request_reload();
            }
            if ui
                .add_enabled(!busy && valid, egui::Button::new("Validate and save"))
                .on_hover_text(
                    "Validate and write this file without restarting the service. Ctrl+S.",
                )
                .clicked()
            {
                self.begin_save(false);
            }
            if theme::primary(ui, "Save and restart", !busy && valid && !custom)
                .on_disabled_hover_text(if custom {
                    "A custom file is not loaded by the service. Save this file without restarting."
                } else {
                    "Finish the current operation and resolve validation errors first."
                })
                .clicked()
            {
                self.begin_save(true);
            }
        });
        if let Err(error) = validation {
            egui::ScrollArea::vertical()
                .id_salt("save-errors")
                .max_height(36.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("Before saving: {error}"))
                                .color(ui.visuals().error_fg_color),
                        )
                        .wrap(),
                    );
                });
        } else if !valid {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "Choose a configuration file in Overview → advanced settings.",
            );
        } else {
            theme::note(
                ui,
                if custom {
                    "Custom file · Save writes this file only; the service will not load it on restart."
                } else {
                    "Save writes the file. Save and restart also applies it to the background service."
                },
            );
        }
    }

    fn draw_validation(&mut self, ui: &mut egui::Ui) {
        theme::heading(
            ui,
            "Test & calibrate.",
            "Read live input from your controller. Fine-tune with confidence.",
        );
        let active = self.validation_task.is_some();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.heading("Live input trace");
                theme::badge(ui, if self.direct_capture { "Direct hardware" } else { "Via daemon" }, self.direct_capture);
                if theme::primary(ui, "Start trace", !active).clicked() {
                    self.start_trace();
                }
            });
            theme::note(ui, if self.direct_capture {
                "Direct mode reads hardware using the saved file. Stop the service before capturing."
            } else {
                "Uses the running daemon's settings; virtual input stays active."
            });
            if !self.direct_capture && self.model.is_explicit_path() {
                theme::note(ui, "The daemon uses its default configuration, not this custom file or its unsaved edits.");
            } else if self.model.dirty {
                theme::note(ui, if self.direct_capture {
                    "Unsaved edits are excluded. Validate and save before testing this file with direct capture."
                } else {
                    "Unsaved edits are excluded. Save and restart to test them through the daemon."
                });
            }
            ui.add_enabled_ui(!active, |ui| {
                field_row(ui, "Controller", |ui, label_id| {
                    ui.add(egui::TextEdit::singleline(&mut self.trace_device)
                        .hint_text("All connected controllers")
                        .desired_width(f32::INFINITY).min_size(egui::vec2(0.0, 30.0)))
                        .labelled_by(label_id).on_hover_text("Leave empty for all controllers, or enter a device path or positive ordinal.").changed()
                });
                field_row(ui, "Event filter", |ui, label_id| {
                    combo_token(ui, "trace-filter", &mut self.trace_filter, &[
                        ("all", "All events"), ("keys", "Buttons"), ("axes", "Axes"), ("ir", "IR sensor"), ("motion-plus", "MotionPlus"),
                    ]).labelled_by(label_id).changed()
                });
            });
            if active {
                theme::note(ui, "Capture is active · Stop capture is always available in the bottom bar.");
            }
            ui.collapsing("Advanced capture options", |ui| {
                ui.add_enabled_ui(!active, |ui| {
                    ui.checkbox(&mut self.direct_capture, "Direct hardware diagnostics (service must be stopped)");
                    ui.add(egui::Label::new(egui::RichText::new(
                        "Direct access can compete with the daemon for hardware. Stop the background service first. This mode reads the saved file, not unsaved edits.")
                        .color(ui.visuals().warn_fg_color)).wrap());
                    if self.direct_capture {
                        field_row(ui, "Temporary profile", |ui, label_id| {
                            let mut token = self.trace_profile.and_then(|p| p.as_str()).unwrap_or("").to_owned();
                            let response = combo_token(ui, "trace-profile", &mut token, &[
                                ("", "Use saved configuration"), ("gamepad", "Gamepad"), ("desktop", "Desktop"), ("both", "Gamepad + desktop"),
                            ]).labelled_by(label_id);
                            self.trace_profile = Profile::parse(&token);
                            response.changed()
                        });
                    }
                });
            });
        });
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Calibrate motion");
            theme::note(ui, "Rest the controller on a flat surface. Capture uses the selected controller, or the first controller when left blank.");
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(!active, egui::Button::new("Capture calibration")).clicked() {
                    self.start_calibration();
                }
                theme::note(ui, &format!("Hold still for {} seconds", self.model.config.aim_calibration_duration));
            });
            theme::note(ui, "Captured values become unsaved edits. Review them in Motion aiming, then save.");
        });
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.collapsing("Saved-file diagnostics & reference", |ui| {
                theme::note(ui, if self.model.dirty {
                    "These checks read the saved file, not your unsaved edits. Results open in the activity log."
                } else {
                    "Readiness and configuration checks use the saved file. Results open in the activity log."
                });
                ui.horizontal_wrapped(|ui| {
                    for (label, arg, sensitive) in [
                        ("Check readiness", "--doctor", true),
                        ("Validate saved file", "--check-config", true),
                        ("Effective settings", "--dump-config", true),
                        ("Input map", "--axis-map", false),
                        ("Test checklist", "--validation-checklist", false),
                    ] {
                        if ui.add_enabled(self.command_task.is_none(), egui::Button::new(label)).clicked() {
                            self.run_command(vec![arg.to_owned()], sensitive);
                        }
                    }
                });
            });
        });
    }

    fn draw_output(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Activity log").strong());
            if ui.button("Copy all").clicked() {
                ui.ctx().copy_text(self.model.output.as_text());
            }
            if ui.button("Clear").clicked() {
                self.model.clear_output();
            }
            if ui.button("Hide log").clicked() {
                self.output_open = false;
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("activity-log")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.model.output.block_count() == 0 {
                    theme::note(
                        ui,
                        "No activity yet. Find devices or check readiness to begin.",
                    );
                } else {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(self.model.output.as_text())
                                .monospace()
                                .size(12.0),
                        )
                        .selectable(true)
                        .wrap(),
                    );
                }
            });
    }

    fn draw_navigation(&mut self, ui: &mut egui::Ui, compact: bool) {
        for (number, tab, name, hint) in [
            ("01", Tab::Overview, "Overview", "Service & connection"),
            ("02", Tab::Configuration, "Configure", "Profiles & movement"),
            (
                "03",
                Tab::Validation,
                "Test & calibrate",
                "Input & calibration",
            ),
        ] {
            let text = if compact {
                name.to_owned()
            } else {
                format!("{number}  {name}\n{hint}")
            };
            let size = if compact {
                egui::vec2(0.0, 34.0)
            } else {
                egui::vec2(ui.available_width(), 60.0)
            };
            if ui
                .add_sized(size, egui::Button::selectable(self.tab == tab, text))
                .clicked()
            {
                self.tab = tab;
            }
        }
    }

    fn draw(&mut self, ctx: &egui::Context) {
        let p = theme::Palette::new(ctx.style().visuals.dark_mode);
        let compact = ctx.content_rect().width() < 1000.0;
        if !self.close_confirmation
            && !self.reload_confirmation
            && self.model.dirty
            && self.config_task.is_none()
            && ctx.input_mut(|input| {
                input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::S,
                ))
            })
        {
            self.begin_save(false);
        }
        if self.emblem.is_none()
            && let Ok(icon) = eframe::icon_data::from_png_bytes(theme::ICON)
        {
            self.emblem = Some(ctx.load_texture(
                "wiiland-emblem",
                egui::ColorImage::from_rgba_unmultiplied(
                    [icon.width as usize, icon.height as usize],
                    &icon.rgba,
                ),
                egui::TextureOptions::LINEAR,
            ));
        }
        if self.model.dirty
            && !self.close_approved
            && ctx.input(|input| input.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_confirmation = true;
        }
        egui::TopBottomPanel::top("app-header")
            .frame(theme::panel(p.surface, 12))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if let Some(texture) = &self.emblem {
                        ui.add(egui::Image::new(texture).fit_to_exact_size(egui::vec2(32.0, 32.0)));
                    }
                    ui.label(
                        egui::RichText::new("wiiland")
                            .size(25.0)
                            .strong()
                            .color(p.ink),
                    );
                    ui.label(
                        egui::RichText::new("CONTROL CENTER")
                            .size(10.0)
                            .color(p.muted),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("Appearance", |ui| {
                            let mut preference = ctx.options(|o| o.theme_preference);
                            for (value, name) in [
                                (egui::ThemePreference::System, "Follow system"),
                                (egui::ThemePreference::Light, "Pearl · light"),
                                (egui::ThemePreference::Dark, "Dusk · dark"),
                            ] {
                                if ui.selectable_value(&mut preference, value, name).clicked() {
                                    ctx.set_theme(preference);
                                    ui.close();
                                }
                            }
                        });
                    });
                });
                if compact {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| self.draw_navigation(ui, true));
                }
            });
        egui::TopBottomPanel::bottom("app-status")
            .frame(theme::panel(p.surface, 10))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            self.output_open,
                            format!("Activity log · {}", self.model.output.block_count()),
                        )
                        .clicked()
                    {
                        self.output_open = !self.output_open;
                    }
                    ui.separator();
                    if let Some(task) = &self.validation_task
                        && ui
                            .add_enabled(!task.cancel_requested, egui::Button::new("Stop capture"))
                            .clicked()
                    {
                        self.stop_capture();
                    }
                    if self.config_task.is_some()
                        || self.command_task.is_some()
                        || self.validation_task.is_some()
                    {
                        ui.spinner();
                    }
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 28.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("status-detail")
                                .max_height(28.0)
                                .show(ui, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&self.status)
                                                .size(12.0)
                                                .color(p.muted),
                                        )
                                        .wrap(),
                                    )
                                    .on_hover_text(&self.status);
                                });
                        },
                    );
                });
            });
        if self.output_open {
            egui::TopBottomPanel::bottom("activity-drawer")
                .resizable(true)
                .default_height(if compact { 140.0 } else { 180.0 })
                .height_range(100.0..=ctx.content_rect().height() * 0.30)
                .frame(theme::panel(p.surface, 10))
                .show(ctx, |ui| self.draw_output(ui));
        }
        egui::TopBottomPanel::bottom("save-bar")
            .frame(theme::panel(p.surface, 10))
            .show(ctx, |ui| self.draw_save_bar(ui));
        if !compact {
            egui::SidePanel::left("navigation")
                .resizable(false)
                .exact_width(192.0)
                .frame(theme::panel(p.surface, 14))
                .show(ctx, |ui| {
                    ui.add_space(12.0);
                    theme::note(ui, "WORKSPACE");
                    ui.add_space(10.0);
                    self.draw_navigation(ui, false);
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                        theme::note(ui, "Wii input for Linux");
                        ui.label(
                            egui::RichText::new(format!("WiiLand {}", env!("CARGO_PKG_VERSION")))
                                .small(),
                        );
                    });
                });
        }
        egui::CentralPanel::default()
            .frame(theme::panel(p.canvas, if compact { 18 } else { 24 }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("page", self.tab, self.config_section))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.set_width(ui.available_width().min(960.0));
                            ui.with_layout(
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| match self.tab {
                                    Tab::Overview => self.draw_overview(ui),
                                    Tab::Configuration => self.draw_configuration(ui),
                                    Tab::Validation => self.draw_validation(ui),
                                },
                            );
                        });
                    });
            });
        if self.close_confirmation || self.reload_confirmation {
            let closing = self.close_confirmation;
            let response = egui::Modal::new(egui::Id::new("unsaved-changes")).show(ctx, |ui| {
                ui.set_width(410.0);
                theme::badge(ui, "Unsaved changes", true);
                ui.add_space(8.0);
                ui.heading(if closing {
                    "Leave without saving?"
                } else {
                    "Reload and discard edits?"
                });
                theme::note(
                    ui,
                    if closing {
                        "Closing discards your local edits. Your saved file stays unchanged."
                    } else {
                        "Reload replaces your local edits with the saved configuration."
                    },
                );
                egui::ScrollArea::vertical()
                    .id_salt("discard-target")
                    .max_height(54.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(self.model.config_path.to_string_lossy())
                                    .small(),
                            )
                            .wrap(),
                        );
                    });
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if theme::primary(ui, "Keep editing", true).clicked() {
                        self.close_confirmation = false;
                        self.reload_confirmation = false;
                    }
                    if ui.button("Discard changes").clicked() {
                        self.close_confirmation = false;
                        self.reload_confirmation = false;
                        if closing {
                            self.close_approved = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        } else {
                            self.begin_load(true);
                        }
                    }
                });
                if let Some(task) = &self.validation_task {
                    ui.separator();
                    if ui
                        .add_enabled(!task.cancel_requested, egui::Button::new("Stop capture"))
                        .clicked()
                    {
                        self.stop_capture();
                    }
                }
            });
            if response.should_close() {
                self.close_confirmation = false;
                self.reload_confirmation = false;
            }
        }
    }
}

impl eframe::App for ControlCenter {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_config_task();
        self.poll_command();
        self.poll_service();
        self.poll_validation();
        if let Some(result) = self.ipc_query.as_ref().and_then(crate::live::Query::poll) {
            if result.is_err()
                && self
                    .ipc_query
                    .as_ref()
                    .is_some_and(crate::live::Query::is_status)
            {
                self.daemon_status = "Daemon unavailable · see activity log".to_owned();
            }
            self.ipc_query = None;
            match result {
                Ok(QueryResult::Status {
                    status,
                    diagnostics,
                    config,
                }) => {
                    self.daemon_status = format!(
                        "wiilandd {} (pid {}): {} device(s)",
                        status.daemon_version, status.pid, status.device_count
                    );
                    self.model.append_output(&format!("{}\ntrace drops={} lifecycle drops={} max pointer lateness={}us max dispatch={}us\nRunning configuration:\n{}", self.daemon_status, diagnostics.trace_records_dropped, diagnostics.lifecycle_records_dropped, diagnostics.max_pointer_lateness_us, diagnostics.max_dispatch_duration_us, config));
                }
                Ok(QueryResult::Devices(devices)) => {
                    for (index, device) in devices.iter().enumerate() {
                        self.model.append_output(&format!(
                            "{}\t{}\t{:?}\n",
                            index + 1,
                            device.syspath,
                            device.profile
                        ));
                    }
                }
                Err(error) => self.model.append_output(&format!("{error}\n")),
            }
        }
        self.draw(ctx);
        let busy = self.config_task.is_some()
            || self.command_task.is_some()
            || self.service_task.is_some()
            || self.validation_task.is_some()
            || self.ipc_query.is_some();
        ctx.request_repaint_after(Duration::from_millis(if busy { 40 } else { 250 }));
    }
}

fn combo_token(
    ui: &mut egui::Ui,
    id: &str,
    value: &mut String,
    choices: &[(&str, &str)],
) -> egui::Response {
    egui::ComboBox::from_id_salt(id)
        .selected_text(if value.is_empty() {
            choices[0].1
        } else {
            choices
                .iter()
                .find(|(token, _)| *token == value)
                .map_or(value.as_str(), |(_, text)| *text)
        })
        .show_ui(ui, |ui| {
            for (token, text) in choices {
                ui.selectable_value(value, (*token).to_owned(), *text);
            }
        })
        .response
}
fn poll_process(task: &ProcessTask, model: &mut ConfigModel) -> Option<ProcessResult> {
    loop {
        match task.try_recv() {
            Ok(Some(ProcessEvent::Stdout(bytes) | ProcessEvent::Stderr(bytes))) => {
                model.append_output(&String::from_utf8_lossy(&bytes));
            }
            Ok(Some(ProcessEvent::Finished(result))) => return Some(result),
            Ok(None) => return None,
            Err(_) => {
                return Some(ProcessResult::unavailable("process result channel closed"));
            }
        }
    }
}

fn decode_capture_result(
    result: ProcessResult,
    kind: ValidationKind,
) -> Result<CaptureResult, String> {
    if !result.success {
        return Err(result
            .error
            .unwrap_or_else(|| format!("Capture process failed (exit {:?})", result.code)));
    }
    if kind == ValidationKind::Trace {
        return Ok(CaptureResult::TraceStopped);
    }
    decode_calibration(&result.stdout).map(CaptureResult::Calibrated)
}

fn decode_calibration(bytes: &[u8]) -> Result<CalibrationResult, String> {
    let config = wiiland_core::Config::parse_bytes("calibration-output", bytes)
        .map_err(|error| error.to_string())?;
    let value = CalibrationResult {
        accel: config.aim_accel_zero,
        motion_plus: config.aim_motion_plus_bias,
    };
    if value.accel.is_none() && value.motion_plus.is_none() {
        Err("Capture returned no complete sensor calibration".into())
    } else {
        Ok(value)
    }
}

fn service_query_status(result: &ProcessResult) -> &'static str {
    match std::str::from_utf8(&result.stdout).map(str::trim) {
        Ok("active") if result.success => "Running",
        Ok("inactive") if result.code == Some(3) => "Stopped",
        Ok("failed") if result.code == Some(3) => "Failed",
        Ok("activating") => "Starting",
        Ok("deactivating") => "Stopping",
        _ => "Unavailable",
    }
}

fn shell_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(char::is_whitespace) {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn initialization_dispatches_load_and_service_status_together() {
        let mut model = ConfigModel::new(PathBuf::from("/tmp/wiiland-initialization-test.conf"));
        model.daemon_path = "/bin/true".to_owned();
        let application = ControlCenter::initialize(model);

        assert!(application.config_task.is_some());
        assert_eq!(
            application
                .model
                .transaction
                .as_ref()
                .map(|transaction| transaction.kind),
            Some(TransactionKind::Load)
        );
        assert_eq!(
            application
                .service_task
                .as_ref()
                .map(|(action, _)| action.as_str()),
            Some("is-active")
        );
        assert_eq!(application.model.next_transaction, 1);
    }

    #[test]
    fn streamed_process_output_reaches_model_before_completion() {
        let task = ProcessTask::spawn(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "printf visible-early; sleep 0.2; printf visible-late".to_owned(),
            ],
        );
        let mut model = ConfigModel::new(PathBuf::from("/tmp/unused.conf"));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut completed = false;
        while Instant::now() < deadline && !model.output.as_text().contains("visible-early") {
            completed = poll_process(&task, &mut model).is_some();
            if !completed {
                thread::sleep(Duration::from_millis(5));
            }
        }
        assert!(model.output.as_text().contains("visible-early"));
        assert!(
            !completed,
            "output was only made visible after process exit"
        );
    }
    #[test]
    fn direct_calibration_rejects_partial_or_invalid_triples_before_applying() {
        for bytes in [
            b"aim-accel-zero-x=10\n".as_slice(),
            b"aim-accel-zero-x=10\naim-accel-zero-y=no\naim-accel-zero-z=30\n".as_slice(),
            b"# no stable samples\n".as_slice(),
        ] {
            let mut application = ControlCenter::new(ConfigModel::new(PathBuf::from(
                "/tmp/unused-calibration.conf",
            )));
            let before = application.model.config.clone();
            let transaction = application.model.begin_calibration().unwrap();
            application.complete_calibration(
                CalibrationOwnership {
                    transaction,
                    device: String::new(),
                },
                decode_calibration(bytes),
            );
            assert_eq!(application.model.config, before);
            assert!(application.model.begin_calibration().is_some());
        }
    }

    #[test]
    fn edited_calibration_completion_is_discarded() {
        let mut application = ControlCenter::new(ConfigModel::new(PathBuf::from(
            "/tmp/calibration-edit.conf",
        )));
        let ownership = CalibrationOwnership {
            transaction: application
                .model
                .begin_calibration()
                .expect("calibration starts"),
            device: String::new(),
        };
        application.model.config.pointer_speed += 1;
        application.model.mark_dirty();
        let edited = application.model.config.clone();

        application.complete_calibration(
            ownership,
            decode_calibration(
                b"aim-accel-zero-x=101\naim-accel-zero-y=102\naim-accel-zero-z=103\n",
            ),
        );

        assert_eq!(application.model.config, edited);
    }

    #[test]
    fn retargeted_calibration_completion_is_discarded() {
        let mut application = ControlCenter::new(ConfigModel::new(PathBuf::from(
            "/tmp/calibration-first.conf",
        )));
        let ownership = CalibrationOwnership {
            transaction: application
                .model
                .begin_calibration()
                .expect("calibration starts"),
            device: String::new(),
        };
        application
            .model
            .set_path(PathBuf::from("/tmp/calibration-second.conf"));
        let retargeted = application.model.config.clone();

        application.complete_calibration(
            ownership,
            decode_calibration(b"aim-motion-plus-bias-x=11\naim-motion-plus-bias-y=12\naim-motion-plus-bias-z=13\n"),
        );

        assert_eq!(application.model.config, retargeted);
    }

    #[test]
    fn queued_save_restart_runs_after_any_active_service_action() {
        for active_action in ["is-active", "stop"] {
            let mut application =
                ControlCenter::new(ConfigModel::new(PathBuf::from("/tmp/service-queue.conf")));
            application.service_program = "/bin/true";
            application.dispatch_service_action(active_action);
            application.request_restart_after_save();
            assert_eq!(
                application.pending_service_actions.pending,
                Some(ServiceAction::Restart)
            );

            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline
                && application
                    .service_task
                    .as_ref()
                    .is_some_and(|(action, _)| action != "restart")
            {
                application.poll_service();
                thread::sleep(Duration::from_millis(5));
            }

            assert_eq!(
                application
                    .service_task
                    .as_ref()
                    .map(|(action, _)| action.as_str()),
                Some("restart"),
                "restart did not dispatch after {active_action}"
            );
            assert_eq!(application.pending_service_actions.pending, None);
        }
    }
}

#[cfg(test)]
#[path = "ui_tests.rs"]
mod interaction_tests;
