#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Features {
    pub gui: bool,
    pub tui: bool,
    pub integrations: bool,
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalDirs {
    pub prefix: PathBuf,
    pub exec_prefix: PathBuf,
    pub bindir: PathBuf,
    pub datadir: PathBuf,
    pub sysconfdir: PathBuf,
    pub docdir: PathBuf,
    pub mandir: PathBuf,
}

#[cfg(unix)]
#[derive(Clone, Debug, Default)]
pub struct LogicalDirOverrides {
    pub prefix: Option<PathBuf>,
    pub exec_prefix: Option<PathBuf>,
    pub bindir: Option<PathBuf>,
    pub datadir: Option<PathBuf>,
    pub sysconfdir: Option<PathBuf>,
    pub docdir: Option<PathBuf>,
    pub mandir: Option<PathBuf>,
}

#[cfg(unix)]
impl LogicalDirOverrides {
    pub fn resolve(self) -> LogicalDirs {
        let defaults = LogicalDirs::default();
        let prefix = self.prefix.unwrap_or(defaults.prefix);
        let exec_prefix = self.exec_prefix.unwrap_or_else(|| prefix.clone());
        let datadir = self.datadir.unwrap_or_else(|| prefix.join("share"));
        LogicalDirs {
            bindir: self.bindir.unwrap_or_else(|| exec_prefix.join("bin")),
            sysconfdir: self.sysconfdir.unwrap_or(defaults.sysconfdir),
            docdir: self.docdir.unwrap_or_else(|| datadir.join("doc/wiiland")),
            mandir: self.mandir.unwrap_or_else(|| datadir.join("man")),
            prefix,
            exec_prefix,
            datadir,
        }
    }
}

#[cfg(unix)]
impl Default for LogicalDirs {
    fn default() -> Self {
        let prefix = PathBuf::from("/usr/local");
        let exec_prefix = prefix.clone();
        Self {
            bindir: exec_prefix.join("bin"),
            datadir: prefix.join("share"),
            sysconfdir: PathBuf::from("/etc"),
            docdir: prefix.join("share/doc/wiiland"),
            mandir: prefix.join("share/man"),
            prefix,
            exec_prefix,
        }
    }
}

#[cfg(unix)]
impl LogicalDirs {
    pub fn validate(&self) -> io::Result<()> {
        for (name, path) in [
            ("prefix", &self.prefix),
            ("exec_prefix", &self.exec_prefix),
            ("bindir", &self.bindir),
            ("datadir", &self.datadir),
            ("sysconfdir", &self.sysconfdir),
            ("docdir", &self.docdir),
            ("mandir", &self.mandir),
        ] {
            if !path.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} must be absolute: {}", path.display()),
                ));
            }
            if path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{name} contains '..'"),
                ));
            }
        }
        if self.sysconfdir != Path::new("/etc") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "sysconfdir must be /etc until runtime config relocation is supported: {}",
                    self.sysconfdir.display()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OptionalDir {
    Auto,
    No,
    Absolute(PathBuf),
}

#[cfg(unix)]
impl OptionalDir {
    pub fn parse(value: &str) -> io::Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "no" => Ok(Self::No),
            _ => {
                let path = PathBuf::from(value);
                if !path.is_absolute()
                    || path
                        .components()
                        .any(|c| c == std::path::Component::ParentDir)
                {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("expected auto, no, or an absolute path: {value}"),
                    ))
                } else {
                    Ok(Self::Absolute(path))
                }
            }
        }
    }

    fn resolve(
        &self,
        package: &str,
        variable: &str,
        fallback: PathBuf,
        suffix: Option<&str>,
    ) -> Option<PathBuf> {
        let path = match self {
            Self::No => return None,
            Self::Absolute(path) => path.clone(),
            Self::Auto => {
                let output = Command::new("pkg-config")
                    .args(["--variable", variable, package])
                    .output()
                    .ok();
                let queried = output
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                queried.unwrap_or(fallback)
            }
        };
        Some(match suffix {
            Some(suffix) if path.file_name() != Some(OsStr::new(suffix)) => path.join(suffix),
            _ => path,
        })
    }
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub enum ItemSource {
    Root(PathBuf),
    Built(PathBuf),
    GeneratedService,
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct Item {
    pub source: ItemSource,
    pub destination: PathBuf,
    pub mode: u32,
}

#[cfg(unix)]
#[derive(Clone, Debug)]
pub struct Manifest {
    pub root: PathBuf,
    pub dirs: LogicalDirs,
    pub features: Features,
    pub udev_dir: Option<PathBuf>,
    pub systemd_dir: Option<PathBuf>,
    pub xorg_dir: Option<PathBuf>,
}

#[cfg(unix)]
impl Manifest {
    pub fn new(
        root: PathBuf,
        dirs: LogicalDirs,
        features: Features,
        udev: OptionalDir,
        systemd: OptionalDir,
        xorg: OptionalDir,
    ) -> io::Result<Self> {
        dirs.validate()?;
        let udev_dir = udev.resolve(
            "udev",
            "udevdir",
            dirs.prefix.join("lib/udev"),
            Some("rules.d"),
        );
        let systemd_dir = systemd.resolve(
            "systemd",
            "systemduserunitdir",
            dirs.prefix.join("lib/systemd/user"),
            None,
        );
        let xorg_dir = xorg.resolve(
            "xorg-server",
            "sysconfigdir",
            dirs.datadir.join("X11/xorg.conf.d"),
            None,
        );
        Ok(Self {
            root,
            dirs,
            features,
            udev_dir,
            systemd_dir,
            xorg_dir,
        })
    }

    pub fn items(&self, profile: &str) -> io::Result<Vec<Item>> {
        let mut items = Vec::new();
        let built =
            |name: &str| ItemSource::Built(self.root.join("target").join(profile).join(name));
        let root = |path: &str| ItemSource::Root(self.root.join(path));
        items.push(Item {
            source: built("wiilandd"),
            destination: self.dirs.bindir.join("wiilandd"),
            mode: 0o755,
        });
        items.push(Item {
            source: built("wiilandd-hardware-report"),
            destination: self.dirs.bindir.join("wiilandd-hardware-report"),
            mode: 0o755,
        });
        if self.features.tui {
            items.push(Item {
                source: built("wiiland-show"),
                destination: self.dirs.bindir.join("wiiland-show"),
                mode: 0o755,
            });
        }
        if self.features.gui {
            items.push(Item {
                source: built("wiiland-config"),
                destination: self.dirs.bindir.join("wiiland-config"),
                mode: 0o755,
            });
        }

        items.push(Item {
            source: root("res/wiilandd.conf"),
            destination: self.dirs.sysconfdir.join("wiiland/wiilandd.conf"),
            mode: 0o644,
        });
        items.push(Item {
            source: root("res/wiilandd.conf"),
            destination: self.dirs.docdir.join("examples/wiilandd.conf"),
            mode: 0o644,
        });

        for name in ["LICENSE", "COPYING"] {
            items.push(Item {
                source: root(name),
                destination: self.dirs.docdir.join(name),
                mode: 0o644,
            });
        }

        for (name, section) in [("wiiland.7", "7"), ("wiilandd.1", "1")] {
            items.push(Item {
                source: root(&format!("doc/{name}")),
                destination: self.dirs.mandir.join(format!("man{section}/{name}")),
                mode: 0o644,
            });
        }
        if self.features.tui {
            items.push(Item {
                source: root("doc/wiiland-show.1"),
                destination: self.dirs.mandir.join("man1/wiiland-show.1"),
                mode: 0o644,
            });
        }
        if self.features.gui {
            items.push(Item {
                source: root("doc/wiiland-config.1"),
                destination: self.dirs.mandir.join("man1/wiiland-config.1"),
                mode: 0o644,
            });
        }
        self.add_recursive_docs(&mut items)?;

        if self.features.integrations {
            if let Some(dir) = &self.udev_dir {
                for name in ["70-udev-wiiland.rules", "70-wiiland-uinput.rules"] {
                    items.push(Item {
                        source: root(&format!("res/{name}")),
                        destination: dir.join(name),
                        mode: 0o644,
                    });
                }
            }
            if let Some(dir) = &self.systemd_dir {
                items.push(Item {
                    source: ItemSource::GeneratedService,
                    destination: dir.join("wiilandd.service"),
                    mode: 0o644,
                });
            }
            if let Some(dir) = &self.xorg_dir {
                items.push(Item {
                    source: root("res/50-xorg-fix-wiiland.conf"),
                    destination: dir.join("50-xorg-fix-wiiland.conf"),
                    mode: 0o644,
                });
            }
        }
        if self.features.gui {
            items.push(Item {
                source: root("res/io.github.philosophimoonbeam.wiiland-config.desktop"),
                destination: self
                    .dirs
                    .datadir
                    .join("applications/io.github.philosophimoonbeam.wiiland-config.desktop"),
                mode: 0o644,
            });
            items.push(Item {
                source: root("res/io.github.philosophimoonbeam.wiiland.svg"),
                destination: self
                    .dirs
                    .datadir
                    .join("icons/hicolor/scalable/apps/io.github.philosophimoonbeam.wiiland.svg"),
                mode: 0o644,
            });
        }
        Ok(items)
    }

    fn add_recursive_docs(&self, items: &mut Vec<Item>) -> io::Result<()> {
        let dir = self.root.join("doc");
        if !dir.is_dir() {
            return Ok(());
        }
        let mut files = Vec::new();
        collect_files(&dir, &mut files)?;
        files.sort();
        for file in files {
            if file
                .extension()
                .and_then(OsStr::to_str)
                .is_some_and(|e| e.chars().all(|c| c.is_ascii_digit()))
            {
                continue;
            }
            let relative = file
                .strip_prefix(&dir)
                .map_err(|_| io::Error::other("documentation path escaped root"))?;
            items.push(Item {
                source: ItemSource::Root(file.strip_prefix(&self.root).unwrap().to_path_buf()),
                destination: self.dirs.docdir.join(relative),
                mode: 0o644,
            });
        }
        Ok(())
    }
}

#[cfg(unix)]
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect_files(&path, files)?;
        } else if ty.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn logical_dirs_keep_runtime_sysconfdir_when_prefix_changes() {
        let dirs = LogicalDirOverrides {
            prefix: Some(PathBuf::from("/opt/wiiland")),
            ..LogicalDirOverrides::default()
        }
        .resolve();

        assert_eq!(dirs.sysconfdir, Path::new("/etc"));
        dirs.validate().unwrap();
    }

    #[test]
    fn logical_dirs_reject_relocated_sysconfdir() {
        let dirs = LogicalDirs {
            sysconfdir: PathBuf::from("/opt/wiiland/etc"),
            ..LogicalDirs::default()
        };

        let error = dirs.validate().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("sysconfdir must be /etc"));
    }
}
