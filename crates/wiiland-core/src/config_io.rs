//! Filesystem and environment adapter for the pure configuration parser.
//! The `Config::load_*` methods remain compatibility entry points.
#[cfg(not(windows))]
use crate::config::SYSTEM_CONFIG_PATH;
use crate::config::{Config, ConfigError};
#[cfg(windows)]
use known_folders::{KnownFolder, get_known_folder_path};
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
        #[cfg(windows)]
        let system = app_config_path(known_folder_path(KnownFolder::ProgramData, "ProgramData")?);
        #[cfg(not(windows))]
        let system = PathBuf::from(SYSTEM_CONFIG_PATH);

        #[cfg(windows)]
        let user = Some(app_config_path(known_folder_path(
            KnownFolder::LocalAppData,
            "LocalAppData",
        )?));
        #[cfg(not(windows))]
        let user = user_config_path();

        Self::load_layers(
            Some(system.as_path()),
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

#[cfg(windows)]
fn app_config_path(mut base: PathBuf) -> PathBuf {
    base.push("wiiland");
    base.push("wiilandd.conf");
    base
}

#[cfg(windows)]
fn windows_path_error(folder: &str, message: String) -> ConfigError {
    ConfigError {
        path: PathBuf::from(folder),
        line: None,
        message,
        source: None,
    }
}

#[cfg(windows)]
fn checked_known_folder_path(
    path: Option<PathBuf>,
    folder_name: &str,
) -> Result<PathBuf, ConfigError> {
    let path = path.ok_or_else(|| {
        windows_path_error(
            folder_name,
            format!("Known Folder API could not resolve {folder_name}"),
        )
    })?;
    if !path.is_absolute() {
        return Err(windows_path_error(
            folder_name,
            "Known Folder API returned a non-absolute path".into(),
        ));
    }
    Ok(path)
}

#[cfg(windows)]
fn known_folder_path(folder: KnownFolder, folder_name: &str) -> Result<PathBuf, ConfigError> {
    checked_known_folder_path(get_known_folder_path(folder), folder_name)
}

#[cfg(windows)]
pub fn user_config_path() -> Option<PathBuf> {
    known_folder_path(KnownFolder::LocalAppData, "LocalAppData")
        .ok()
        .map(app_config_path)
}

#[cfg(not(windows))]
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
    let mut path = PathBuf::from(base);
    path.push("wiiland");
    path.push("wiilandd.conf");
    Some(path)
}
#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn known_folder_path_preserves_non_ascii_user_path() {
        let base = checked_known_folder_path(
            Some(PathBuf::from("C:\\Users\\Zoë\\AppData\\Local")),
            "LocalAppData",
        )
        .unwrap();
        assert_eq!(
            app_config_path(base),
            PathBuf::from("C:\\Users\\Zoë\\AppData\\Local\\wiiland\\wiilandd.conf")
        );
    }

    #[test]
    fn known_folder_lookup_failure_is_reported_without_fallback() {
        let error = checked_known_folder_path(None, "ProgramData").unwrap_err();
        assert_eq!(
            error.message,
            "Known Folder API could not resolve ProgramData"
        );
        assert_eq!(error.path, PathBuf::from("ProgramData"));
        assert_eq!(error.line, None);
    }
}
