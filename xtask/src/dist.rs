#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Component;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const ROOT: &str = "wiiland-2";
#[cfg(unix)]
const DIRECTORY_MODE: u32 = 0o755;
#[cfg(unix)]
const SOURCE_DIRS: &[&str] = &[".cargo", ".github", ".omp", "crates", "doc", "res", "xtask"];
#[cfg(unix)]
const SOURCE_FILES: &[&str] = &[
    ".gitignore",
    "Cargo.lock",
    "Cargo.toml",
    "COPYING",
    "DEV",
    "LICENSE",
    "README.md",
    "rust-toolchain.toml",
];

#[cfg(unix)]
fn collect(
    source: &Path,
    relative: &Path,
    excluded: &Path,
    files: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let current = source.join(relative);
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let name = entry.file_name();
        let rel = relative.join(&name);
        if source.join(&rel) == excluded
            || name == ".git"
            || name == ".hg"
            || name == ".svn"
            || name == "target"
            || name == "autom4te.cache"
            || name == ".deps"
            || name == ".libs"
            || name == "__pycache__"
        {
            continue;
        }
        let archive_or_debris = name.to_string_lossy();
        if archive_or_debris.ends_with(".tar")
            || archive_or_debris.ends_with(".tar.gz")
            || archive_or_debris.ends_with(".tar.xz")
            || archive_or_debris.ends_with(".tar.bz2")
            || archive_or_debris.ends_with(".tar.zst")
            || archive_or_debris.ends_with(".tgz")
            || archive_or_debris.ends_with(".tbz2")
            || archive_or_debris.ends_with(".txz")
            || archive_or_debris.ends_with(".zip")
            || archive_or_debris.ends_with(".crate")
            || archive_or_debris.ends_with(".tmp")
            || archive_or_debris.ends_with('~')
        {
            continue;
        }
        let ty = entry.file_type()?;
        if ty.is_dir() {
            collect(source, &rel, excluded, files)?;
        } else if ty.is_file() || ty.is_symlink() {
            files.push(rel);
        }
    }
    Ok(())
}

#[cfg(unix)]
fn collect_sources(source: &Path, excluded: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for &directory in SOURCE_DIRS {
        collect(source, Path::new(directory), excluded, &mut files)?;
    }
    for &file in SOURCE_FILES {
        let relative = PathBuf::from(file);
        if source.join(&relative) == excluded {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("archive output overlaps required source: {file}"),
            ));
        }
        files.push(relative);
    }
    files.sort();
    Ok(files)
}

#[cfg(unix)]
fn copy_entry(source: &Path, dest: &Path, relative: &Path) -> io::Result<()> {
    let from = source.join(relative);
    let to = dest.join(relative);
    let ty = fs::symlink_metadata(&from)?.file_type();
    if ty.is_symlink() {
        let target = fs::read_link(&from)?;
        if target.is_absolute() || target.components().any(|c| c == Component::ParentDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe source symlink: {}", relative.display()),
            ));
        }
        fs::create_dir_all(to.parent().unwrap())?;
        std::os::unix::fs::symlink(target, to)?;
    } else {
        fs::create_dir_all(to.parent().unwrap())?;
        fs::copy(&from, &to)?;
        let mode = fs::symlink_metadata(&from)?.permissions().mode() & !0o022 & 0o777;
        fs::set_permissions(&to, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(unix)]
fn normalize_directory_modes(root: &Path) -> io::Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        fs::set_permissions(&directory, fs::Permissions::from_mode(DIRECTORY_MODE))?;
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn verification_script() -> &'static [u8] {
    br##"#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"
if command -v cargo >/dev/null 2>&1; then
    cargo check --workspace --all-targets --all-features --locked
    cargo test --workspace --all-targets --all-features --locked
    exec cargo xtask build --release --features gui,tui,integrations
fi
printf '%s\n' 'cargo is required to verify this source archive' >&2
exit 127
"##
}

#[cfg(unix)]
fn unique_temp(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

#[cfg(unix)]
fn dist_unix(root: &Path, output: &Path) -> io::Result<()> {
    let source = fs::canonicalize(root)?;
    let output = if output.is_absolute() {
        output.to_path_buf()
    } else {
        env::current_dir()?.join(output)
    };
    let excluded = fs::canonicalize(&output).unwrap_or_else(|_| output.clone());
    let stage = unique_temp("wiiland-dist");
    let tree = stage.join(ROOT);
    let mut tmp_name = output
        .file_name()
        .unwrap_or_else(|| OsStr::new("archive"))
        .to_os_string();
    tmp_name.push(".");
    tmp_name.push(stage.file_name().unwrap());
    tmp_name.push(".tmp");
    let tmp = output.with_file_name(tmp_name);

    let result = (|| {
        fs::create_dir_all(&tree)?;
        let files = collect_sources(&source, &excluded)?;
        for relative in files {
            copy_entry(&source, &tree, &relative)?;
        }
        let verify = tree.join("verify-dist.sh");
        fs::write(&verify, verification_script())?;
        fs::set_permissions(&verify, fs::Permissions::from_mode(0o755))?;
        normalize_directory_modes(&tree)?;

        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        let status = Command::new("tar")
            .args([
                "--sort=name",
                "--mtime=@0",
                "--owner=0",
                "--group=0",
                "--numeric-owner",
                "--format=ustar",
                "-cJf",
            ])
            .arg(&tmp)
            .arg("-C")
            .arg(&stage)
            .arg(ROOT)
            .env("XZ_OPT", "-9e")
            .status()?;
        if !status.success() {
            return Err(io::Error::other("tar failed while creating source archive"));
        }
        fs::rename(&tmp, &output)
    })();

    let tmp_cleanup = match fs::remove_file(&tmp) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    };
    let stage_cleanup = match fs::remove_dir_all(&stage) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    };
    match result {
        Ok(()) => {
            tmp_cleanup?;
            stage_cleanup
        }
        Err(error) => Err(error),
    }
}
pub fn dist(root: &Path, output: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        dist_unix(root, output)
    }
    #[cfg(not(unix))]
    {
        let _ = (root, output);
        Err(unsupported_unix_packaging())
    }
}

#[cfg(not(unix))]
fn unsupported_unix_packaging() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix distribution and verification are unsupported on this platform; use packaging/windows for the Windows installer",
    )
}

#[cfg(unix)]
fn list_archive(archive: &Path) -> io::Result<Vec<String>> {
    let output = Command::new("tar").args(["-tJf"]).arg(archive).output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unable to read source archive",
        ));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "archive listing is not UTF-8"))?;
    Ok(text.lines().map(str::to_owned).collect())
}

#[cfg(unix)]
fn validate_names(names: &[String]) -> io::Result<()> {
    let mut roots = std::collections::BTreeSet::new();
    for name in names {
        let path = Path::new(name);
        if name.is_empty()
            || path.is_absolute()
            || name.contains('\0')
            || path.components().any(|c| c == Component::ParentDir)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe archive entry: {name:?}"),
            ));
        }
        let mut components = path.components();
        if components.next() != Some(Component::Normal(OsStr::new(ROOT))) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "archive must contain exactly one wiiland-2 root",
            ));
        }
        if let Some(Component::Normal(root)) = path.components().next() {
            roots.insert(root.to_owned());
        }
    }
    if roots.len() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archive has more than one root",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_dist_unix(archive: &Path) -> io::Result<()> {
    let names = list_archive(archive)?;
    validate_names(&names)?;
    if !names.iter().any(|name| name == "wiiland-2/verify-dist.sh") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archive lacks verification entrypoint",
        ));
    }
    let extraction = unique_temp("wiiland-verify");
    let result = (|| {
        fs::create_dir(&extraction)?;
        let status = Command::new("tar")
            .args(["--no-same-owner", "--no-same-permissions", "-xJf"])
            .arg(archive)
            .arg("-C")
            .arg(&extraction)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("unable to extract source archive"));
        }
        let script = extraction.join(ROOT).join("verify-dist.sh");
        let status = Command::new(&script)
            .current_dir(extraction.join(ROOT))
            .stdin(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("extracted source verification failed"));
        }
        Ok(())
    })();
    let cleanup = fs::remove_dir_all(&extraction);
    match result {
        Ok(()) => cleanup,
        Err(error) => Err(error),
    }
}
pub fn verify_dist(archive: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        verify_dist_unix(archive)
    }
    #[cfg(not(unix))]
    {
        let _ = archive;
        Err(unsupported_unix_packaging())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = unique_temp("wiiland-dist-test");
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn source_tree() -> TempDir {
        let temp = TempDir::new();
        for directory in SOURCE_DIRS {
            fs::create_dir_all(temp.path().join(directory)).unwrap();
        }
        for file in SOURCE_FILES {
            let path = temp.path().join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file).unwrap();
        }
        temp
    }

    #[test]
    fn collection_uses_the_top_level_allowlist_and_excludes_custom_output() {
        let source = source_tree();
        fs::write(source.path().join(".omp/config.json"), b"{}").unwrap();
        fs::write(source.path().join("res/kept.txt"), b"kept").unwrap();
        let output = source.path().join("res/custom-output");
        fs::write(&output, b"old archive").unwrap();
        fs::write(source.path().join("root-debris"), b"debris").unwrap();
        fs::create_dir(source.path().join("src")).unwrap();
        fs::write(source.path().join("src/excluded.c"), b"excluded").unwrap();

        let files = collect_sources(source.path(), &output).unwrap();

        assert!(files.contains(&PathBuf::from(".omp/config.json")));
        assert!(files.contains(&PathBuf::from("Cargo.toml")));
        assert!(files.contains(&PathBuf::from("res/kept.txt")));
        for excluded in ["res/custom-output", "root-debris", "src/excluded.c"] {
            assert!(
                !files.contains(&PathBuf::from(excluded)),
                "unexpected source archive entry: {excluded}"
            );
        }
    }

    #[test]
    fn collection_is_sorted_deterministically() {
        let source = source_tree();
        for file in ["z-last", "m-middle", "a-first"] {
            fs::write(source.path().join(".omp").join(file), file).unwrap();
        }

        let files = collect_sources(source.path(), &source.path().join("output")).unwrap();
        let mut expected = files.clone();
        expected.sort();

        assert_eq!(files, expected);
    }

    #[test]
    fn staged_directory_modes_do_not_depend_on_creation_umask() {
        let temp = TempDir::new();
        for (tree_name, inherited_mode) in [("umask-022", 0o755), ("umask-077", 0o700)] {
            let tree = temp.path().join(tree_name);
            let nested = tree.join("crates/example/src");
            fs::create_dir_all(&nested).unwrap();
            for directory in [
                &tree,
                &tree.join("crates"),
                &tree.join("crates/example"),
                &nested,
            ] {
                fs::set_permissions(directory, fs::Permissions::from_mode(inherited_mode)).unwrap();
            }

            normalize_directory_modes(&tree).unwrap();

            for directory in [
                &tree,
                &tree.join("crates"),
                &tree.join("crates/example"),
                &nested,
            ] {
                assert_eq!(
                    fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                    DIRECTORY_MODE,
                    "non-deterministic mode for {}",
                    directory.display()
                );
            }
        }
    }

    #[test]
    fn archive_names_require_one_safe_wiiland_root() {
        let valid = [
            "wiiland-2/",
            "wiiland-2/.omp/config.json",
            "wiiland-2/Cargo.toml",
        ]
        .map(str::to_owned);
        assert!(validate_names(&valid).is_ok());

        for invalid in [
            Vec::new(),
            vec!["other-root/file".to_owned()],
            vec!["wiiland-2/file".to_owned(), "other-root/file".to_owned()],
            vec!["wiiland-2/../outside".to_owned()],
            vec!["../wiiland-2/file".to_owned()],
            vec!["/wiiland-2/file".to_owned()],
            vec!["".to_owned()],
        ] {
            assert!(
                validate_names(&invalid).is_err(),
                "accepted unsafe archive names: {invalid:?}"
            );
        }
    }

    #[test]
    fn verification_script_checks_tests_then_builds_the_extracted_tree() {
        let script = str::from_utf8(verification_script()).unwrap();
        let lines: Vec<_> = script.lines().collect();
        let check = "    cargo check --workspace --all-targets --all-features --locked";
        let test = "    cargo test --workspace --all-targets --all-features --locked";
        let build = "    exec cargo xtask build --release --features gui,tui,integrations";
        let check_index = lines.iter().position(|line| *line == check).unwrap();
        let test_index = lines.iter().position(|line| *line == test).unwrap();
        let build_index = lines.iter().position(|line| *line == build).unwrap();

        assert_eq!(&lines[..2], ["#!/bin/sh", "set -eu"]);
        assert!(check_index < test_index);
        assert!(test_index < build_index);
        assert!(lines.contains(&"if command -v cargo >/dev/null 2>&1; then"));
        assert!(
            lines.contains(&"printf '%s\\n' 'cargo is required to verify this source archive' >&2")
        );
        assert_eq!(lines.last(), Some(&"exit 127"));
    }
}
