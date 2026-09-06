//! Filesystem and environment adapter for the pure configuration parser.
//! The `Config::load_*` methods remain compatibility entry points.
use crate::config::{Config, ConfigError, SYSTEM_CONFIG_PATH};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

impl Config {
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_default_layers()
    }
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let mut c = Self::default();
        read_layer(&mut c, path.as_ref(), true)?;
        c.validate()?;
        Ok(c)
    }
    pub fn load_layers(
        system: Option<impl AsRef<Path>>,
        user: Option<impl AsRef<Path>>,
        explicit: Option<impl AsRef<Path>>,
    ) -> Result<Self, ConfigError> {
        let mut c = Self::default();
        if let Some(path) = explicit {
            read_layer(&mut c, path.as_ref(), true)?;
        } else {
            if let Some(path) = system {
                read_layer(&mut c, path.as_ref(), false)?;
            }
            if let Some(path) = user {
                read_layer(&mut c, path.as_ref(), false)?;
            }
        }
        c.validate()?;
        Ok(c)
    }
    pub fn load_default_layers() -> Result<Self, ConfigError> {
        let user = user_config_path();
        Self::load_layers(
            Some(Path::new(SYSTEM_CONFIG_PATH)),
            user.as_deref(),
            Option::<&Path>::None,
        )
    }
}
fn read_layer(config: &mut Config, path: &Path, required: bool) -> Result<(), ConfigError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if !required && error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ConfigError::io(path, error)),
    };
    config.apply_bytes(path, &bytes)
}

pub fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| Path::new(v).is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| Path::new(v).is_absolute())
                .map(|v| {
                    let mut p = PathBuf::from(v);
                    p.push(".config");
                    p.into_os_string()
                })
        })?;
    let mut p = PathBuf::from(base);
    p.push("wiiland");
    p.push("wiilandd.conf");
    Some(p)
}
