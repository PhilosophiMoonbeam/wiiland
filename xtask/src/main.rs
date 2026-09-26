mod dist;
#[cfg(unix)]
mod install;
mod manifest;

#[cfg(unix)]
use install::{InstallOptions, install, uninstall};
use manifest::Features;
#[cfg(unix)]
use manifest::{LogicalDirOverrides, LogicalDirs, Manifest, OptionalDir};
use std::env;
use std::io;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, ExitCode};

#[derive(Debug)]
enum Command {
    Build {
        release: bool,
        features: Features,
    },
    Check {
        all_features: bool,
    },
    #[cfg(unix)]
    Install {
        options: InstallOptions,
        dirs: LogicalDirs,
        features: Features,
        udev: OptionalDir,
        systemd: OptionalDir,
        xorg: OptionalDir,
    },
    #[cfg(unix)]
    Uninstall {
        destdir: PathBuf,
        dirs: LogicalDirs,
    },
    Docs,
    Dist {
        output: PathBuf,
    },
    VerifyDist {
        archive: PathBuf,
    },
}

fn usage() -> &'static str {
    "usage: cargo xtask <build|check|install|uninstall|docs|dist|verify-dist> [options]\n\n\
build options: --features gui,tui,integrations --all-features --release\n\
install options: --destdir PATH --prefix PATH --exec-prefix PATH --bindir PATH\n  --datadir PATH --sysconfdir PATH --docdir PATH --mandir PATH\n  --with-udev-rules-dir auto|no|PATH --with-systemd-user-unit-dir auto|no|PATH\n  --with-xorg-conf-dir auto|no|PATH --features gui,tui,integrations --debug\n"
}

fn parse_features(value: &str) -> io::Result<Features> {
    let mut features = Features::default();
    for feature in value.split(',').filter(|feature| !feature.is_empty()) {
        match feature {
            "gui" => features.gui = true,
            "tui" => features.tui = true,
            "integrations" => features.integrations = true,
            "default" => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown feature: {feature}"),
                ));
            }
        }
    }
    Ok(features)
}

fn take_value(args: &[String], index: &mut usize, option: &str) -> io::Result<String> {
    *index += 1;
    args.get(*index).cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} requires a value"),
        )
    })
}

fn parse(args: &[String]) -> io::Result<Command> {
    let Some(name) = args.first().map(String::as_str) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, usage()));
    };
    match name {
        "build" | "check" => {
            let mut release = false;
            let mut all_features = false;
            let mut features = Features::default();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--release" => release = true,
                    "--all-features" => {
                        all_features = true;
                        features = Features {
                            gui: true,
                            tui: true,
                            integrations: true,
                        };
                    }
                    "--features" => {
                        features = parse_features(&take_value(args, &mut i, "--features")?)?;
                    }
                    option => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown option {option}"),
                        ));
                    }
                }
                i += 1;
            }
            if name == "build" {
                Ok(Command::Build { release, features })
            } else {
                Ok(Command::Check { all_features })
            }
        }
        #[cfg(unix)]
        "install" => {
            let mut dir_overrides = LogicalDirOverrides::default();
            let mut features = Features {
                integrations: true,
                ..Features::default()
            };
            let mut options = InstallOptions::default();
            let mut udev = OptionalDir::Auto;
            let mut systemd = OptionalDir::Auto;
            let mut xorg = OptionalDir::Auto;
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--destdir" => {
                        options.destdir = PathBuf::from(take_value(args, &mut i, "--destdir")?)
                    }
                    "--prefix" => {
                        dir_overrides.prefix =
                            Some(PathBuf::from(take_value(args, &mut i, "--prefix")?))
                    }
                    "--exec-prefix" => {
                        dir_overrides.exec_prefix =
                            Some(PathBuf::from(take_value(args, &mut i, "--exec-prefix")?))
                    }
                    "--bindir" => {
                        dir_overrides.bindir =
                            Some(PathBuf::from(take_value(args, &mut i, "--bindir")?))
                    }
                    "--datadir" => {
                        dir_overrides.datadir =
                            Some(PathBuf::from(take_value(args, &mut i, "--datadir")?))
                    }
                    "--sysconfdir" => {
                        dir_overrides.sysconfdir =
                            Some(PathBuf::from(take_value(args, &mut i, "--sysconfdir")?))
                    }
                    "--docdir" => {
                        dir_overrides.docdir =
                            Some(PathBuf::from(take_value(args, &mut i, "--docdir")?))
                    }
                    "--mandir" => {
                        dir_overrides.mandir =
                            Some(PathBuf::from(take_value(args, &mut i, "--mandir")?))
                    }
                    "--with-udev-rules-dir" => {
                        udev =
                            OptionalDir::parse(&take_value(args, &mut i, "--with-udev-rules-dir")?)?
                    }
                    "--with-systemd-user-unit-dir" => {
                        systemd = OptionalDir::parse(&take_value(
                            args,
                            &mut i,
                            "--with-systemd-user-unit-dir",
                        )?)?
                    }
                    "--with-xorg-conf-dir" => {
                        xorg =
                            OptionalDir::parse(&take_value(args, &mut i, "--with-xorg-conf-dir")?)?
                    }
                    "--features" => {
                        features = parse_features(&take_value(args, &mut i, "--features")?)?
                    }
                    "--debug" => options.profile = "debug".to_owned(),
                    "--release" => options.profile = "release".to_owned(),
                    option => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown option {option}"),
                        ));
                    }
                }
                i += 1;
            }
            let dirs = dir_overrides.resolve();
            dirs.validate()?;
            Ok(Command::Install {
                options,
                dirs,
                features,
                udev,
                systemd,
                xorg,
            })
        }
        #[cfg(unix)]
        "uninstall" => {
            let mut destdir = PathBuf::new();
            let mut dir_overrides = LogicalDirOverrides::default();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--destdir" => destdir = PathBuf::from(take_value(args, &mut i, "--destdir")?),
                    "--prefix" => {
                        dir_overrides.prefix =
                            Some(PathBuf::from(take_value(args, &mut i, "--prefix")?))
                    }
                    "--exec-prefix" => {
                        dir_overrides.exec_prefix =
                            Some(PathBuf::from(take_value(args, &mut i, "--exec-prefix")?))
                    }
                    "--bindir" => {
                        dir_overrides.bindir =
                            Some(PathBuf::from(take_value(args, &mut i, "--bindir")?))
                    }
                    "--datadir" => {
                        dir_overrides.datadir =
                            Some(PathBuf::from(take_value(args, &mut i, "--datadir")?))
                    }
                    "--sysconfdir" => {
                        dir_overrides.sysconfdir =
                            Some(PathBuf::from(take_value(args, &mut i, "--sysconfdir")?))
                    }
                    "--docdir" => {
                        dir_overrides.docdir =
                            Some(PathBuf::from(take_value(args, &mut i, "--docdir")?))
                    }
                    "--mandir" => {
                        dir_overrides.mandir =
                            Some(PathBuf::from(take_value(args, &mut i, "--mandir")?))
                    }
                    option => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown option {option}"),
                        ));
                    }
                }
                i += 1;
            }
            let dirs = dir_overrides.resolve();
            dirs.validate()?;
            Ok(Command::Uninstall { destdir, dirs })
        }
        "docs" => Ok(Command::Docs),
        "dist" => {
            let mut output = PathBuf::from("wiiland-2.tar.xz");
            if args.len() > 1 {
                if args.len() != 3 || args[1] != "--output" {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "dist accepts only --output PATH",
                    ));
                }
                output = PathBuf::from(&args[2]);
            }
            Ok(Command::Dist { output })
        }
        "verify-dist" => {
            let archive = args
                .get(1)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("wiiland-2.tar.xz"));
            if args.len() > 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "verify-dist accepts one archive path",
                ));
            }
            Ok(Command::VerifyDist { archive })
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidInput, usage())),
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

const BASE_BUILD_PACKAGES: [&str; 4] = ["wiiland-ipc", "wiiland-hid", "wiilandd", "wiiland-dump"];

fn build_packages(features: Features) -> Vec<&'static str> {
    let mut packages = BASE_BUILD_PACKAGES.to_vec();
    if features.gui {
        packages.push("wiiland-config");
    }
    if features.tui {
        packages.push("wiiland-show");
    }
    packages
}

fn run_build(release: bool, features: Features) -> io::Result<()> {
    let mut process = ProcessCommand::new("cargo");
    process.current_dir(root()).arg("build");
    for package in build_packages(features) {
        process.args(["--package", package]);
    }
    if release {
        process.arg("--release");
    }
    let status = process.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("cargo build failed"))
    }
}

fn run_cargo(command: &str, release: bool) -> io::Result<()> {
    let mut process = ProcessCommand::new("cargo");
    process.current_dir(root()).arg(command).arg("--workspace");
    if release {
        process.arg("--release");
    }
    let status = process.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("cargo {command} failed")))
    }
}

fn main_result() -> io::Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    #[cfg(not(unix))]
    if matches!(
        args.first().map(String::as_str),
        Some("install" | "uninstall")
    ) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix install and uninstall are unsupported on this platform; use packaging/windows for the Windows installer",
        ));
    }
    let command = parse(&args)?;
    match command {
        Command::Build { release, features } => run_build(release, features),
        Command::Check { all_features } => {
            let mut process = ProcessCommand::new("cargo");
            process
                .current_dir(root())
                .args(["check", "--workspace", "--all-targets"]);
            if all_features {
                process.arg("--all-features");
            }
            let status = process.status()?;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("cargo check failed"))
            }
        }
        #[cfg(unix)]
        Command::Install {
            options,
            dirs,
            features,
            udev,
            systemd,
            xorg,
        } => install(
            &Manifest::new(root(), dirs, features, udev, systemd, xorg)?,
            &options,
        ),
        #[cfg(unix)]
        Command::Uninstall { destdir, dirs } => uninstall(
            &Manifest::new(
                root(),
                dirs,
                Features {
                    integrations: true,
                    ..Features::default()
                },
                OptionalDir::Auto,
                OptionalDir::Auto,
                OptionalDir::Auto,
            )?,
            &destdir,
        ),
        Command::Docs => run_cargo("doc", false),
        Command::Dist { output } => dist::dist(&root(), &output),
        Command::VerifyDist { archive } => dist::verify_dist(&archive),
    }
}

fn main() -> ExitCode {
    match main_result() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::Path;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[cfg(unix)]
    #[test]
    fn install_parse_keeps_canonical_runtime_sysconfdir() {
        let command = parse(&args(&["install", "--prefix", "/opt/wiiland"])).unwrap();

        let Command::Install { dirs, .. } = command else {
            panic!("expected install command");
        };
        assert_eq!(dirs.sysconfdir, Path::new("/etc"));
    }

    #[cfg(unix)]
    #[test]
    fn install_parse_rejects_relocated_sysconfdir() {
        let error = parse(&args(&["install", "--sysconfdir", "/opt/wiiland/etc"])).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("sysconfdir must be /etc"));
    }

    #[cfg(unix)]
    fn parsed_dirs(values: &[&str]) -> LogicalDirs {
        match parse(&args(values)).unwrap() {
            Command::Install { dirs, .. } | Command::Uninstall { dirs, .. } => dirs,
            _ => panic!("expected install or uninstall command"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn logical_directory_overrides_are_independent_of_argument_order() {
        for command in ["install", "uninstall"] {
            let prefix_before_exec = parsed_dirs(&[
                command,
                "--prefix",
                "/opt/wiiland",
                "--exec-prefix",
                "/opt/wiiland/host",
            ]);
            let exec_before_prefix = parsed_dirs(&[
                command,
                "--exec-prefix",
                "/opt/wiiland/host",
                "--prefix",
                "/opt/wiiland",
            ]);
            assert_eq!(prefix_before_exec, exec_before_prefix);
            assert_eq!(
                prefix_before_exec.bindir,
                Path::new("/opt/wiiland/host/bin")
            );
            assert_eq!(prefix_before_exec.datadir, Path::new("/opt/wiiland/share"));
            assert_eq!(prefix_before_exec.sysconfdir, Path::new("/etc"));
        }
    }

    #[test]
    fn build_features_select_only_requested_optional_packages() {
        let command = parse(&args(&["build", "--features", "gui"])).unwrap();
        let Command::Build { features, .. } = command else {
            panic!("expected build command");
        };

        assert_eq!(
            build_packages(features),
            vec![
                "wiiland-ipc",
                "wiiland-hid",
                "wiilandd",
                "wiiland-dump",
                "wiiland-config"
            ]
        );
        assert_eq!(
            build_packages(Features {
                integrations: true,
                ..Features::default()
            }),
            BASE_BUILD_PACKAGES
        );
    }

    #[test]
    fn build_all_features_selects_both_optional_surfaces() {
        let command = parse(&args(&["build", "--all-features"])).unwrap();
        let Command::Build { features, .. } = command else {
            panic!("expected build command");
        };

        assert_eq!(
            build_packages(features),
            vec![
                "wiiland-ipc",
                "wiiland-hid",
                "wiilandd",
                "wiiland-dump",
                "wiiland-config",
                "wiiland-show"
            ]
        );
    }
}
