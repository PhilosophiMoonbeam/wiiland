use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use wiiland_core::{
    AimActivation, AimMode, AimSource, DesktopAction, DeviceRuleKind, IrAimMapping, IrTracking,
    Profile, SensorCalibration,
};

use crate::config_task::{ConfigEvent, ConfigTask};
use crate::live::{CalibrationResult, CaptureResult, QueryResult};
use crate::model::{self, ApplyCompletion, CalibrationTransaction, ConfigModel, TransactionKind};
use crate::process::{self, ProcessEvent, ProcessResult, ProcessTask};
use crate::theme;

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
        let p = theme::Palette::for_ui(ui);
        theme::card(ui)
            .fill(p.mist)
            .stroke(egui::Stroke::NONE)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let illustration_size = if ui.available_width() > 700.0 {
                        186.0
                    } else {
                        120.0
                    };
                    if let Some(texture) = &self.emblem {
                        ui.add(
                            egui::Image::new(texture).fit_to_exact_size(egui::vec2(
                                illustration_size,
                                illustration_size,
                            )),
                        );
                    }
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new("WELCOME TO WIILAND")
                                .size(11.0)
                                .color(p.muted),
                        );
                        ui.label(
                            egui::RichText::new("A familiar controller.\nA new home.")
                                .size(30.0)
                                .color(p.ink),
                        );
                        theme::note(
                            ui,
                            "Play, point, and move. Make Wii input feel at home on Linux.",
                        );
                        if theme::primary(ui, "Set up your controls", true).clicked() {
                            self.tab = Tab::Configuration;
                        }
                    });
                });
            });
        ui.add_space(10.0);
        if ui.available_width() >= 720.0 {
            ui.columns(2, |columns| {
                self.draw_service_card(&mut columns[0]);
                self.draw_discovery_card(&mut columns[1]);
            });
        } else {
            self.draw_service_card(ui);
            ui.add_space(8.0);
            self.draw_discovery_card(ui);
        }
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Configuration location");
            theme::note(ui, if self.model.is_explicit_path() {
                "Custom file. Save here to export settings; the background service uses its own configuration."
            } else {
                "Your settings layer over the system defaults. Save and restart to apply them to the service."
            });
            ui.add_space(4.0);
            ui.label("Configuration file");
            let mut path = self.model.config_path.to_string_lossy().into_owned();
            if ui.add(egui::TextEdit::singleline(&mut path).desired_width(f32::INFINITY).min_size(egui::vec2(0.0, 34.0))).changed() {
                self.model.set_path(PathBuf::from(path));
            }
            ui.collapsing("Advanced connection settings", |ui| {
                ui.label("Daemon executable");
                if ui.add(egui::TextEdit::singleline(&mut self.model.daemon_path).desired_width(f32::INFINITY).min_size(egui::vec2(0.0, 34.0))).changed() {
                    self.model.revision = self.model.revision.wrapping_add(1);
                }
                theme::note(ui, &format!("Window system: {}", Self::backend_name()));
            });
            if ui.add_enabled(self.config_task.is_none(), egui::Button::new("Reload saved settings")).clicked() {
                self.request_reload();
            }
        });
    }

    fn draw_service_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Background service");
            theme::note(ui, &self.daemon_status);
            if ui.add_enabled(self.ipc_query.is_none(), egui::Button::new("Refresh daemon status")).clicked() {
                self.ipc_query = Some(crate::live::Query::start(false));
                self.output_open = true;
            }
            ui.horizontal_wrapped(|ui| {
                if self.service_task.is_some() {
                    ui.spinner();
                }
                theme::badge(
                    ui,
                    &self.service_status,
                    matches!(self.service_status.as_str(), "Unavailable" | "Failed"),
                );
            });
            theme::note(ui, if self.service_status == "Unavailable" {
                "The user service is unavailable. Check readiness for setup and permission details."
            } else { "Runs your saved input settings in the background." });
            ui.horizontal_wrapped(|ui| {
                for (label, action) in [
                    ("Start", "start"),
                    ("Stop", "stop"),
                    ("Restart", "restart"),
                    ("Refresh", "is-active"),
                ] {
                    if ui
                        .add_enabled(self.service_task.is_none(), egui::Button::new(label))
                        .clicked()
                    {
                        self.service_action(action);
                    }
                }
            });
        });
    }

    fn draw_discovery_card(&mut self, ui: &mut egui::Ui) {
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Get connected");
            theme::note(
                ui,
                "Connect a controller, then check device discovery and input permissions.",
            );
            ui.horizontal_wrapped(|ui| {
                if theme::primary(ui, "Find devices", self.command_task.is_none()).clicked() {
                    self.run_command(vec!["--list".to_owned(), "--verbose".to_owned()], false);
                }
                if ui
                    .add_enabled(
                        self.command_task.is_none(),
                        egui::Button::new("Check readiness"),
                    )
                    .clicked()
                {
                    self.run_command(vec!["--doctor".to_owned()], true);
                }
            });
            theme::note(ui, "Results appear in the activity log.");
        });
    }

    fn draw_configuration(&mut self, ui: &mut egui::Ui) {
        theme::heading(
            ui,
            "Make it feel like you.",
            "Choose a profile, tune movement, and give every button a purpose.",
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
        if let Err(error) = validation {
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Before saving: {error}"),
            );
        } else if !valid {
            ui.colored_label(
                ui.visuals().error_fg_color,
                "Choose a configuration file on the Overview page.",
            );
        }
        ui.horizontal_wrapped(|ui| {
            theme::badge(ui, if self.model.dirty { "Unsaved changes" } else { "No pending edits" }, self.model.dirty);
            if busy { ui.spinner(); }
            if ui.add_enabled(!busy, egui::Button::new("Reload")).on_hover_text("Reload saved configuration from disk.").clicked() {
                self.request_reload();
            }
            if ui.add_enabled(!busy && valid, egui::Button::new("Validate and save")).clicked() {
                self.begin_save(false);
            }
            if theme::primary(ui, "Save and restart", !busy && valid && !self.model.is_explicit_path())
                .on_disabled_hover_text("Available for the default user configuration after validation passes and pending work finishes.").clicked() {
                self.begin_save(true);
            }
        });
        if self.model.is_explicit_path() {
            theme::note(
                ui,
                "Custom file: saving writes this file only. The service will not load it on restart.",
            );
        }
    }

    fn draw_validation(&mut self, ui: &mut egui::Ui) {
        theme::heading(
            ui,
            "See every movement.",
            "Inspect live input or capture a steady starting point for motion aiming.",
        );
        let active = self.validation_task.is_some();
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Live input trace");
            theme::note(ui, "Capture sensors from the running daemon. Its virtual input remains active. Calibration uses the first controller when no selector is given.");
            ui.add_enabled(!active, egui::Checkbox::new(&mut self.direct_capture, "Direct hardware diagnostics (service must be stopped)"));
            ui.add_enabled_ui(!active, |ui| {
                field_row(ui, "Controller", |ui, label_id| {
                    ui.add(egui::TextEdit::singleline(&mut self.trace_device)
                        .hint_text("All connected controllers")
                        .desired_width(f32::INFINITY).min_size(egui::vec2(0.0, 34.0)))
                        .labelled_by(label_id).on_hover_text("Leave empty for all controllers, or enter a device path or positive ordinal.").changed()
                });
                field_row(ui, "Event filter", |ui, label_id| {
                    combo_token(ui, "trace-filter", &mut self.trace_filter, &[
                        ("all", "All events"), ("keys", "Buttons"), ("axes", "Axes"), ("ir", "IR sensor"), ("motion-plus", "MotionPlus"),
                    ]).labelled_by(label_id).changed()
                });
                if self.direct_capture { field_row(ui, "Temporary profile", |ui, label_id| {
                    let mut token = self.trace_profile.and_then(|p| p.as_str()).unwrap_or("").to_owned();
                    let response = combo_token(ui, "trace-profile", &mut token, &[
                        ("", "Use saved configuration"), ("gamepad", "Gamepad"), ("desktop", "Desktop"), ("both", "Gamepad + desktop"),
                    ]).labelled_by(label_id);
                    self.trace_profile = Profile::parse(&token);
                    response.changed()
                }); }
            });
            ui.horizontal_wrapped(|ui| {
                if theme::primary(ui, "Start trace", !active).clicked() { self.start_trace(); }
                if ui.add_enabled(active, egui::Button::new("Stop capture")).clicked() {
                    self.stop_capture();
                }
                if active { ui.spinner(); theme::note(ui, "Capture running · see activity log"); }
            });
        });
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Find your steady point");
            theme::note(ui, "Place the controller on a flat surface and keep it still during capture. Captured values become unsaved edits; save them when you are ready.");
            ui.label(format!("Capture duration: {} seconds", self.model.config.aim_calibration_duration));
            if ui.add_enabled(!active, egui::Button::new("Capture calibration")).clicked() { self.start_calibration(); }
        });
        ui.add_space(10.0);
        theme::card(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.heading("Diagnostics & reference");
            theme::note(
                ui,
                "These checks read the saved configuration. Results appear in the activity log.",
            );
            ui.horizontal_wrapped(|ui| {
                for (label, arg, sensitive) in [
                    ("Check readiness", "--doctor", true),
                    ("Validate saved file", "--check-config", true),
                    ("Effective settings", "--dump-config", true),
                    ("Input map", "--axis-map", false),
                    ("Test checklist", "--validation-checklist", false),
                ] {
                    if ui
                        .add_enabled(self.command_task.is_none(), egui::Button::new(label))
                        .clicked()
                    {
                        self.run_command(vec![arg.to_owned()], sensitive);
                    }
                }
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
        egui::ScrollArea::both().id_salt("activity-log").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if self.model.output.block_count() == 0 {
                theme::note(ui, "Command results will appear here. Start with Find devices or Check readiness.");
            } else {
                ui.add(egui::Label::new(egui::RichText::new(self.model.output.as_text()).monospace()).selectable(true).extend());
            }
        });
    }

    fn draw_navigation(&mut self, ui: &mut egui::Ui, compact: bool) {
        for (tab, name, hint) in [
            (Tab::Overview, "Overview", "Service & connection"),
            (Tab::Configuration, "Configure", "Profiles & movement"),
            (
                Tab::Validation,
                "Test & calibrate",
                "Live input & diagnostics",
            ),
        ] {
            let text = if compact {
                name.to_owned()
            } else {
                format!("{name}\n{hint}")
            };
            let size = if compact {
                egui::vec2(0.0, 34.0)
            } else {
                egui::vec2(ui.available_width(), 62.0)
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
            .frame(theme::panel(p.surface, 16))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("wiiland").size(26.0).color(p.accent));
                    ui.label(egui::RichText::new("/  control center").color(p.muted));
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
                    ui.horizontal_wrapped(|ui| self.draw_navigation(ui, true));
                }
            });
        egui::TopBottomPanel::bottom("app-status")
            .frame(theme::panel(p.surface, 10))
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
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
                    if self.validation_task.is_some() && ui.button("Stop capture").clicked() {
                        self.stop_capture();
                    }
                    if self.config_task.is_some()
                        || self.command_task.is_some()
                        || self.validation_task.is_some()
                    {
                        ui.spinner();
                    }
                    ui.label(egui::RichText::new(&self.status).size(12.0).color(p.muted));
                });
            });
        if self.output_open {
            egui::TopBottomPanel::bottom("activity-drawer")
                .resizable(true)
                .default_height(170.0)
                .height_range(100.0..=ctx.content_rect().height() * 0.35)
                .frame(theme::panel(p.surface, 14))
                .show(ctx, |ui| self.draw_output(ui));
        }
        if self.tab == Tab::Configuration || self.model.dirty {
            egui::TopBottomPanel::bottom("save-bar")
                .frame(theme::panel(p.surface, 14))
                .show(ctx, |ui| self.draw_save_bar(ui));
        }
        if !compact {
            egui::SidePanel::left("navigation")
                .resizable(false)
                .exact_width(216.0)
                .frame(theme::panel(p.surface, 18))
                .show(ctx, |ui| {
                    ui.add_space(20.0);
                    theme::note(ui, "CONTROL CENTER");
                    ui.add_space(6.0);
                    self.draw_navigation(ui, false);
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                        theme::note(ui, "Native Wii input for Linux");
                        ui.label(
                            egui::RichText::new(format!("WiiLand {}", env!("CARGO_PKG_VERSION")))
                                .small(),
                        );
                    });
                });
        }
        egui::CentralPanel::default()
            .frame(theme::panel(p.canvas, if compact { 20 } else { 28 }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("page", self.tab, self.config_section))
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match self.tab {
                            Tab::Overview => self.draw_overview(ui),
                            Tab::Configuration => self.draw_configuration(ui),
                            Tab::Validation => self.draw_validation(ui),
                        }
                    });
            });
        if self.close_confirmation || self.reload_confirmation {
            let closing = self.close_confirmation;
            let response = egui::Modal::new(egui::Id::new("unsaved-changes")).show(ctx, |ui| {
                ui.set_width(390.0);
                ui.heading(if closing {
                    "Leave without saving?"
                } else {
                    "Reload and discard edits?"
                });
                ui.label("Your unsaved configuration changes will be lost.");
                ui.add_space(8.0);
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

fn draw_profile(ui: &mut egui::Ui, model: &mut ConfigModel) {
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.heading("How would you like to play?");
        theme::note(ui, "Gamepad for games. Desktop for a pointer and keyboard. Both keeps each available.");
        ui.add_space(8.0);
        if combo_profile(ui, "Default profile", &mut model.config.profile) {
            model.mark_dirty();
        }
        ui.add_space(8.0);
        ui.separator();
        ui.heading("Pointer feel");
        theme::note(ui, "Tune desktop pointer movement. Higher smoothing reduces jitter and adds a little delay.");
        if drag(
            ui,
            "D-pad pointer speed",
            &mut model.config.pointer_speed,
            1,
            127,
        ) {
            model.mark_dirty();
        }
        if drag(ui, "IR pointer gain", &mut model.config.ir_speed, 1, 127) {
            model.mark_dirty();
        }
        if drag(
            ui,
            "IR jitter deadzone",
            &mut model.config.ir_deadzone,
            0,
            127,
        ) {
            model.mark_dirty();
        }
        if drag(ui, "IR smoothing %", &mut model.config.ir_smoothing, 0, 95) {
            model.mark_dirty();
        }
        if combo_enum(
            ui,
            "IR tracking",
            &mut model.config.ir_tracking,
            [
                (IrTracking::Dual, "Sensor-bar pair"),
                (IrTracking::Centroid, "Visible-point centroid"),
                (IrTracking::First, "First visible point"),
            ],
        ) {
            model.mark_dirty();
        }
        if combo_enum(
            ui,
            "IR aim mapping",
            &mut model.config.ir_aim_mapping,
            [
                (IrAimMapping::Relative, "Relative movement"),
                (IrAimMapping::Absolute, "Absolute screen position"),
            ],
        ) {
            model.mark_dirty();
        }
        let mut enabled = model.config.ir_screen.is_some();
        if ui
            .checkbox(&mut enabled, "Use screen calibration")
            .changed()
        {
            model.config.ir_screen = enabled.then(model::ir_rect_default);
            model.mark_dirty();
        }
        let mut rect = model
            .config
            .ir_screen
            .unwrap_or_else(model::ir_rect_default);
        for (name, value) in [
            ("IR screen left", &mut rect.left),
            ("IR screen right", &mut rect.right),
            ("IR screen top", &mut rect.top),
            ("IR screen bottom", &mut rect.bottom),
        ] {
            if enabled && drag(ui, name, value, 0, 32767) {
                model.mark_dirty();
            }
        }
        if enabled {
            model.config.ir_screen = Some(rect);
        }
    });
}

fn draw_aim(ui: &mut egui::Ui, model: &mut ConfigModel) {
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.heading("Movement, made natural.");
        theme::note(ui, "Turn motion into a right stick or mouse pointer. Choose a sensor and how aiming is activated.");
        ui.add_space(8.0);
        if combo_enum(
            ui,
            "Output",
            &mut model.config.aim_mode,
            [
                (AimMode::Off, "Off"),
                (AimMode::RightStick, "Right stick"),
                (AimMode::Mouse, "Mouse pointer"),
            ],
        ) {
            model.mark_dirty();
        }
        if combo_enum(
            ui,
            "Best available sensor",
            &mut model.config.aim_source,
            [
                (AimSource::Auto, "Automatic"),
                (AimSource::Ir, "IR sensor"),
                (AimSource::MotionPlus, "MotionPlus"),
                (AimSource::Accelerometer, "Accelerometer"),
            ],
        ) {
            model.mark_dirty();
        }
        if combo_enum(
            ui,
            "Activation",
            &mut model.config.aim_activation,
            [
                (AimActivation::B, "B button"),
                (AimActivation::Always, "Always active"),
                (AimActivation::Z, "Nunchuk Z"),
                (AimActivation::C, "Nunchuk C"),
            ],
        ) {
            model.mark_dirty();
        }
        if drag(ui, "Sensitivity", &mut model.config.aim_sensitivity, 1, 127) {
            model.mark_dirty();
        }
        if drag(ui, "Deadzone", &mut model.config.aim_deadzone, 0, 32767) {
            model.mark_dirty();
        }
        if drag(ui, "Smoothing %", &mut model.config.aim_smoothing, 0, 95) {
            model.mark_dirty();
        }
        if ui
            .checkbox(&mut model.config.aim_invert_x, "Invert X")
            .changed()
        {
            model.mark_dirty();
        }
        if ui
            .checkbox(&mut model.config.aim_invert_y, "Invert Y")
            .changed()
        {
            model.mark_dirty();
        }
        let mut accel = model.config.aim_accel_zero.is_some();
        if ui
            .checkbox(&mut accel, "Use accelerometer calibration")
            .changed()
        {
            model.config.aim_accel_zero = accel.then(model::calibration_default);
            model.mark_dirty();
        }
        let mut motion = model.config.aim_motion_plus_bias.is_some();
        if ui
            .checkbox(&mut motion, "Use MotionPlus calibration")
            .changed()
        {
            model.config.aim_motion_plus_bias = motion.then(model::calibration_default);
            model.mark_dirty();
        }
        if drag(
            ui,
            "Calibration duration",
            &mut model.config.aim_calibration_duration,
            1,
            30,
        ) {
            model.mark_dirty();
        }
        let changed = model
            .config
            .aim_accel_zero
            .as_mut()
            .map(|cal| calibration_fields(ui, "Accelerometer zero", cal))
            .unwrap_or(false);
        if changed {
            model.mark_dirty();
        }
        let changed = model
            .config
            .aim_motion_plus_bias
            .as_mut()
            .map(|cal| calibration_fields(ui, "MotionPlus bias", cal))
            .unwrap_or(false);
        if changed {
            model.mark_dirty();
        }
    });
}

fn calibration_fields(ui: &mut egui::Ui, label: &str, cal: &mut SensorCalibration) -> bool {
    let mut changed = false;
    ui.add_space(8.0);
    ui.label(egui::RichText::new(label).strong());
    for (axis, value) in [("X", &mut cal.x), ("Y", &mut cal.y), ("Z", &mut cal.z)] {
        changed |= drag(ui, axis, value, -32768, 32767);
    }
    changed
}

fn draw_bindings(ui: &mut egui::Ui, model: &mut ConfigModel) {
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.heading("Every button, a purpose.");
        theme::note(ui, "These bindings apply to Desktop and Both profiles. Gamepad buttons keep their normal mapping.");
        ui.add_space(8.0);
        for (name, label) in model::binding_names() {
            let mut value = match name {
                "a" => model.config.desktop_bindings.a,
                "b" => model.config.desktop_bindings.b,
                "plus" => model.config.desktop_bindings.plus,
                "minus" => model.config.desktop_bindings.minus,
                "home" => model.config.desktop_bindings.home,
                "one" => model.config.desktop_bindings.one,
                _ => model.config.desktop_bindings.two,
            };
            let changed = combo_action(ui, label, &mut value);
            match name {
                "a" => model.config.desktop_bindings.a = value,
                "b" => model.config.desktop_bindings.b = value,
                "plus" => model.config.desktop_bindings.plus = value,
                "minus" => model.config.desktop_bindings.minus = value,
                "home" => model.config.desktop_bindings.home = value,
                "one" => model.config.desktop_bindings.one = value,
                _ => model.config.desktop_bindings.two = value,
            }
            if changed {
                model.mark_dirty();
            }
        }
    });
}

fn draw_rules(ui: &mut egui::Ui, model: &mut ConfigModel) {
    theme::card(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.heading("A profile for every controller");
        theme::note(ui, "Match part of a device path or device type. Later matching rules take priority over earlier ones.");
        ui.add_space(4.0);
        if model.config.device_rules.is_empty() {
            theme::badge(ui, "All controllers use the default profile", false);
        }
        let mut remove = None;
        for index in 0..model.config.device_rules.len() {
            let mut rule = model.config.device_rules[index].clone();
            let mut changed = false;
            // Each rule owns its widget IDs, including its combo popups and text field.
            ui.push_id(("device-rule", index), |ui| {
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("Rule {}", index + 1)).strong());
                    if ui.small_button("Remove").clicked() { remove = Some(index); }
                });
                changed |= combo_enum(ui, "Match by", &mut rule.kind, [
                    (DeviceRuleKind::Syspath, "Device path"), (DeviceRuleKind::Devtype, "Device type"),
                ]);
                changed |= field_row(ui, "Contains", |ui, label_id| {
                    ui.add(egui::TextEdit::singleline(&mut rule.match_text).hint_text("Required match text").desired_width(f32::INFINITY).min_size(egui::vec2(0.0, 34.0)))
                        .labelled_by(label_id).changed()
                });
                changed |= combo_profile(ui, "Use profile", &mut rule.profile);
                if rule.match_text.trim().is_empty() {
                    ui.colored_label(ui.visuals().warn_fg_color, "Enter part of the device path or type before saving.");
                }
            });
            if changed {
                model.config.device_rules[index] = rule;
                model.mark_dirty();
            }
        }
        if let Some(index) = remove {
            model.config.device_rules.remove(index);
            model.mark_dirty();
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(model.config.device_rules.len() < model::MAX_DEVICE_RULES, egui::Button::new("Add rule")).clicked() {
                model.config.device_rules.push(model::rule(DeviceRuleKind::Devtype, String::new(), Profile::GAMEPAD));
                model.mark_dirty();
            }
            theme::note(ui, &format!("{} / {} rules", model.config.device_rules.len(), model::MAX_DEVICE_RULES));
        });
    });
}

fn field_row(
    ui: &mut egui::Ui,
    label: &str,
    add: impl FnOnce(&mut egui::Ui, egui::Id) -> bool,
) -> bool {
    let width = ui.available_width();
    let control_width = (width * 0.52).min(300.0);
    ui.horizontal(|ui| {
        let label = ui
            .allocate_ui_with_layout(
                egui::vec2(width - control_width - 10.0, 34.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.set_min_width(width - control_width - 10.0);
                    ui.add(egui::Label::new(label).wrap())
                },
            )
            .inner;
        ui.allocate_ui_with_layout(
            egui::vec2(control_width, 34.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().combo_width = control_width;
                add(ui, label.id)
            },
        )
        .inner
    })
    .inner
}

fn drag(ui: &mut egui::Ui, label: &str, value: &mut i32, min: i32, max: i32) -> bool {
    field_row(ui, label, |ui, label_id| {
        ui.add_sized(
            [ui.available_width(), 34.0],
            egui::DragValue::new(value).range(min..=max).speed(1.0),
        )
        .labelled_by(label_id)
        .changed()
    })
}

fn combo_profile(ui: &mut egui::Ui, label: &str, value: &mut Profile) -> bool {
    combo_enum(ui, label, value, model::profile_choices())
}

fn combo_action(ui: &mut egui::Ui, label: &str, value: &mut DesktopAction) -> bool {
    combo_enum(ui, label, value, model::desktop_actions())
}

fn combo_enum<T: Copy + Eq, const N: usize>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    choices: [(T, &str); N],
) -> bool {
    field_row(ui, label, |ui, label_id| {
        let before = *value;
        let selected = choices
            .iter()
            .find(|(candidate, _)| *candidate == *value)
            .map_or("", |(_, text)| *text);
        egui::ComboBox::from_id_salt(("enum", label))
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for (candidate, text) in &choices {
                    ui.selectable_value(value, *candidate, *text);
                }
            })
            .response
            .labelled_by(label_id);
        *value != before
    })
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
            assert_eq!(application.status, "Calibration failed");
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
        assert!(application.status.contains("discarded"));
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
        assert!(application.status.contains("discarded"));
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
