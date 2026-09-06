#![forbid(unsafe_code)]

pub mod aim;
pub mod calibration;
pub mod config;
pub mod config_io;
pub mod engine;
pub mod mapping;
pub mod pointer;
pub mod trace;

pub use config::{
    AimActivation, AimMode, AimSource, Backend, Config, ConfigError, DesktopAction,
    DesktopBindings, DeviceRule, DeviceRuleKind, IrAimMapping, IrRectangle, IrTracking,
    MAX_DEVICE_RULES, MAX_LINE_BYTES, Profile, SensorCalibration, ValidatedConfig,
};
pub use trace::{
    AbsPayload, EventType, KeyPayload, TraceConfig, TraceEvent, TraceFilter, TracePayload,
    event_type_name, is_abs_event, is_key_event,
};
