use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
pub use wiiland_core::MAX_DEVICE_RULES;
use wiiland_core::{
    AimActivation, AimMode, AimSource, Config, ConfigError, DesktopAction, DeviceRule,
    DeviceRuleKind, IrAimMapping, IrRectangle, IrTracking, Profile, SensorCalibration,
};

pub const OUTPUT_BLOCK_LIMIT: usize = 10_000;
const OUTPUT_BLOCK_BYTE_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionKind {
    Load,
    Save,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transaction {
    pub id: u64,
    pub kind: TransactionKind,
    pub revision: u64,
    pub target: PathBuf,
    pub captured: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalibrationTransaction {
    pub id: u64,
    pub revision: u64,
    pub target: PathBuf,
    pub daemon_program: String,
    pub captured: Config,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub id: u64,
    pub kind: TransactionKind,
    pub revision: u64,
    pub target: PathBuf,
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Bytes rendered for a save. Keeping this in the completion makes a save
    /// independent from later form edits and prevents a stale callback from
    /// serializing the current (rather than captured) form.
    pub captured: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplyCompletion {
    Applied,
    Stale,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutputBuffer {
    blocks: VecDeque<String>,
    bytes: usize,
}

impl Default for OutputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputBuffer {
    pub fn new() -> Self {
        Self {
            blocks: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        for line in text.split_inclusive('\n') {
            let mut remainder = line;
            while !remainder.is_empty() {
                let mut end = remainder.len().min(OUTPUT_BLOCK_BYTE_LIMIT);
                while !remainder.is_char_boundary(end) {
                    end -= 1;
                }
                let (block, rest) = remainder.split_at(end);
                self.blocks.push_back(block.to_owned());
                self.bytes += block.len();
                remainder = rest;
            }
        }
        while self.blocks.len() > OUTPUT_BLOCK_LIMIT {
            if let Some(old) = self.blocks.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
        }
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
        self.bytes = 0;
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }
    #[cfg(test)]
    pub fn byte_count(&self) -> usize {
        self.bytes
    }
    pub fn as_text(&self) -> String {
        self.blocks
            .iter()
            .fold(String::with_capacity(self.bytes), |mut s, b| {
                s.push_str(b);
                s
            })
    }
}

#[derive(Clone, Debug)]
pub struct ConfigModel {
    pub config: Config,
    pub daemon_path: String,
    pub config_path: PathBuf,
    pub revision: u64,
    pub dirty: bool,
    pub transaction: Option<Transaction>,
    calibration_transaction: Option<CalibrationTransaction>,
    pub next_transaction: u64,
    pub output: OutputBuffer,
}

impl Default for ConfigModel {
    fn default() -> Self {
        Self::new(
            default_config_path().unwrap_or_else(|| PathBuf::from("/etc/wiiland/wiilandd.conf")),
        )
    }
}

impl ConfigModel {
    pub fn new(path: PathBuf) -> Self {
        Self {
            config: Config::default(),
            daemon_path: "wiilandd".to_owned(),
            config_path: path,
            revision: 0,
            dirty: false,
            transaction: None,
            calibration_transaction: None,
            next_transaction: 0,
            output: OutputBuffer::new(),
        }
    }
    pub fn append_output(&mut self, text: &str) {
        self.output.append(text);
    }

    pub fn clear_output(&mut self) {
        self.output.clear();
    }

    pub fn default_path() -> Option<PathBuf> {
        default_config_path()
    }

    pub fn daemon_program(&self) -> &str {
        let trimmed = self.daemon_path.trim();
        if trimmed.is_empty() {
            "wiilandd"
        } else {
            trimmed
        }
    }

    pub fn is_explicit_path(&self) -> bool {
        self.is_explicit_target(&self.config_path)
    }

    pub fn is_explicit_target(&self, path: &Path) -> bool {
        normalize_path(path)
            != default_config_path()
                .map(|p| normalize_path(&p))
                .unwrap_or_default()
    }

    pub fn mark_dirty(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.dirty = true;
    }

    pub fn begin(&mut self, kind: TransactionKind, captured: Vec<u8>) -> Option<Transaction> {
        if self.transaction.is_some() {
            return None;
        }
        self.next_transaction = self.next_transaction.wrapping_add(1);
        let transaction = Transaction {
            id: self.next_transaction,
            kind,
            revision: self.revision,
            target: self.config_path.clone(),
            captured,
        };
        self.transaction = Some(transaction.clone());
        Some(transaction)
    }

    pub fn begin_calibration(&mut self) -> Option<CalibrationTransaction> {
        if self.calibration_transaction.is_some() {
            return None;
        }
        self.next_transaction = self.next_transaction.wrapping_add(1);
        let transaction = CalibrationTransaction {
            id: self.next_transaction,
            revision: self.revision,
            target: self.config_path.clone(),
            daemon_program: self.daemon_program().to_owned(),
            captured: self.config.clone(),
        };
        self.calibration_transaction = Some(transaction.clone());
        Some(transaction)
    }

    pub fn finish_calibration(&mut self, transaction: &CalibrationTransaction) -> bool {
        if self.calibration_transaction.as_ref() != Some(transaction) {
            return false;
        }
        let active = self
            .calibration_transaction
            .take()
            .expect("calibration transaction ownership checked");
        self.revision == active.revision
            && self.config_path == active.target
            && self.daemon_program() == active.daemon_program.as_str()
            && self.config == active.captured
    }

    pub fn owns(&self, completion: &Completion) -> bool {
        self.transaction.as_ref().is_some_and(|active| {
            active.id == completion.id
                && active.kind == completion.kind
                && active.target == completion.target
        })
    }

    pub fn finish(&mut self, completion: &Completion) -> ApplyCompletion {
        if !self.owns(completion) {
            return ApplyCompletion::Stale;
        }
        let active = self
            .transaction
            .take()
            .expect("transaction ownership checked");
        if !completion.success {
            return ApplyCompletion::Failed;
        }
        match active.kind {
            TransactionKind::Load => {
                if self.revision != active.revision || self.config_path != active.target {
                    return ApplyCompletion::Stale;
                }
                let Ok(config) = parse_config_bytes(&completion.stdout) else {
                    return ApplyCompletion::Failed;
                };
                self.config = config;
                self.dirty = false;
                ApplyCompletion::Applied
            }
            TransactionKind::Save => {
                // Persist exactly the captured bytes even if the form changed while
                // validation ran. A stale save remains dirty so the newer edits win.
                if persist_atomic(&active.target, &active.captured).is_err() {
                    return ApplyCompletion::Failed;
                }
                if self.revision == active.revision && self.config_path == active.target {
                    self.dirty = false;
                    ApplyCompletion::Applied
                } else {
                    ApplyCompletion::Stale
                }
            }
        }
    }

    pub fn render(&self) -> Vec<u8> {
        render_config(&self.config)
    }

    pub fn validate_form(&self) -> Result<(), String> {
        self.config.validate().map_err(|e| e.to_string())?;
        if self.config.device_rules.len() > MAX_DEVICE_RULES {
            return Err("too many device rules".to_owned());
        }
        for rule in &self.config.device_rules {
            if rule.match_text.trim().is_empty()
                || rule
                    .match_text
                    .chars()
                    .any(|c| matches!(c, '#' | '=' | '\n' | '\r'))
            {
                return Err("rule match text cannot contain #, =, or line breaks".to_owned());
            }
        }
        Ok(())
    }

    pub fn set_path(&mut self, path: PathBuf) {
        if self.config_path != path {
            self.config_path = path;
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

pub fn default_config_path() -> Option<PathBuf> {
    wiiland_core::config_io::user_config_path()
}

fn normalize_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

pub fn parse_config_bytes(bytes: &[u8]) -> Result<Config, ConfigError> {
    Config::parse_bytes("daemon-output", bytes)
}

pub fn render_config(config: &Config) -> Vec<u8> {
    let mut rendered = String::from("# Generated by wiiland-config.\n");
    rendered.push_str(&config.dump());
    rendered.into_bytes()
}

pub fn persist_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map(|_| ()).map_err(|e| e.error)
}

pub fn profile_choices() -> [(Profile, &'static str); 3] {
    [
        (Profile::GAMEPAD, "Gamepad"),
        (Profile::DESKTOP, "Desktop pointer"),
        (Profile::BOTH, "Gamepad + desktop"),
    ]
}
pub fn desktop_actions() -> [(DesktopAction, &'static str); 8] {
    [
        (DesktopAction::LeftClick, "Left click"),
        (DesktopAction::RightClick, "Right click"),
        (DesktopAction::Enter, "Enter"),
        (DesktopAction::Escape, "Escape"),
        (DesktopAction::Overview, "Overview"),
        (DesktopAction::PageUp, "Page up"),
        (DesktopAction::PageDown, "Page down"),
        (DesktopAction::Disabled, "Disabled"),
    ]
}
pub fn binding_names() -> [(&'static str, &'static str); 7] {
    [
        ("a", "A button"),
        ("b", "B button"),
        ("plus", "+ button"),
        ("minus", "− button"),
        ("home", "Home button"),
        ("one", "ONE button"),
        ("two", "TWO button"),
    ]
}
pub fn rule(kind: DeviceRuleKind, text: String, profile: Profile) -> DeviceRule {
    DeviceRule {
        kind,
        match_text: text,
        profile,
    }
}
pub fn ir_rect_default() -> IrRectangle {
    IrRectangle {
        left: 0,
        right: 1023,
        top: 0,
        bottom: 767,
    }
}
pub fn calibration_default() -> SensorCalibration {
    SensorCalibration {
        x: 0,
        y: 0,
        z: 0,
        axes: SensorCalibration::ALL,
    }
}

// Keep these imports and enum values part of this module's public contract: the
// UI uses canonical tokens rather than display labels when rendering controls.
#[allow(dead_code)]
fn _canonical_tokens(_: IrTracking, _: IrAimMapping, _: AimMode, _: AimSource, _: AimActivation) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_retention_is_bounded_for_lines_and_unbroken_streams() {
        let mut output = OutputBuffer::new();
        for _ in 0..(OUTPUT_BLOCK_LIMIT + 2) {
            output.append("line\n");
        }
        assert_eq!(output.block_count(), OUTPUT_BLOCK_LIMIT);

        output.clear();
        output.append(&"x".repeat(OUTPUT_BLOCK_BYTE_LIMIT * (OUTPUT_BLOCK_LIMIT + 2)));
        assert_eq!(output.block_count(), OUTPUT_BLOCK_LIMIT);
        assert_eq!(
            output.byte_count(),
            OUTPUT_BLOCK_BYTE_LIMIT * OUTPUT_BLOCK_LIMIT
        );
    }

    #[test]
    fn calibration_ownership_checks_identity_and_captured_state() {
        let mut model = ConfigModel::new(PathBuf::from("/tmp/calibration-ownership.conf"));
        let transaction = model.begin_calibration().expect("calibration starts");
        let mut impostor = transaction.clone();
        impostor.id = impostor.id.wrapping_add(1);

        assert!(!model.finish_calibration(&impostor));

        model.config.aim_calibration_duration += 1;
        assert!(!model.finish_calibration(&transaction));
    }
}
