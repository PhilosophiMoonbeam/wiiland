use crate::manifest::Manifest;
#[cfg(unix)]
use crate::manifest::{Item, ItemSource};
#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::fs::{self, File, OpenOptions};
use std::io;
#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process;

#[derive(Clone, Debug)]
pub struct InstallOptions {
    pub destdir: PathBuf,
    pub profile: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            destdir: PathBuf::new(),
            profile: "release".to_owned(),
        }
    }
}

#[cfg(unix)]
fn staged(destdir: &Path, logical: &Path) -> PathBuf {
    if destdir.as_os_str().is_empty() {
        logical.to_path_buf()
    } else {
        destdir.join(logical.strip_prefix("/").unwrap_or(logical))
    }
}

#[cfg(unix)]
fn atomic_write(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("destination has no parent"))?;
    let tmp = parent.join(format!(
        ".{}{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("wiiland"),
        process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
fn source_bytes(item: &Item, manifest: &Manifest) -> io::Result<Vec<u8>> {
    match &item.source {
        ItemSource::Root(path) | ItemSource::Built(path) => fs::read(path),
        ItemSource::GeneratedService => Ok(format!(
            "[Unit]\n\
             Description=WiiLand input bridge\n\
             Documentation=man:wiilandd(1)\n\
             \n\
             [Service]\n\
             Type=simple\n\
             NoNewPrivileges=yes\n\
             LockPersonality=yes\n\
             MemoryDenyWriteExecute=yes\n\
             RestrictRealtime=yes\n\
             RestrictSUIDSGID=yes\n\
             SystemCallArchitectures=native\n\
             UMask=0077\n\
             ExecCondition={}/wiilandd --check-config\n\
             ExecStart={}/wiilandd\n\
             Restart=on-failure\n\
             RestartSec=2s\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            manifest.dirs.bindir.display(),
            manifest.dirs.bindir.display()
        )
        .into_bytes()),
    }
}

#[cfg(unix)]
fn marker_path(manifest: &Manifest, destdir: &Path) -> PathBuf {
    staged(
        destdir,
        &manifest.dirs.sysconfdir.join("wiiland/.xtask-install"),
    )
}

#[cfg(unix)]
fn ensure_parent_dirs(
    destdir: &Path,
    logical_target: &Path,
    owned_dirs: &mut BTreeSet<PathBuf>,
) -> io::Result<()> {
    let parent = logical_target
        .parent()
        .ok_or_else(|| io::Error::other("destination has no parent"))?;
    let mut ancestors = parent.ancestors().collect::<Vec<_>>();
    ancestors.reverse();
    for logical in ancestors {
        let target = staged(destdir, logical);
        match fs::create_dir(&target) {
            Ok(()) => {
                owned_dirs.insert(logical.to_path_buf());
            }
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists
                    && fs::metadata(&target).is_ok_and(|metadata| metadata.is_dir()) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn install(manifest: &Manifest, options: &InstallOptions) -> io::Result<()> {
    #[cfg(unix)]
    {
        let items = manifest.items(&options.profile)?;
        install_items(manifest, options, &items)
    }
    #[cfg(not(unix))]
    {
        let _ = (manifest, options);
        Err(unsupported_unix_packaging())
    }
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum OwnershipRecord {
    File { logical: PathBuf, expected: String },
    Directory(PathBuf),
}

#[cfg(unix)]
fn read_records(marker: &Path) -> io::Result<Option<Vec<OwnershipRecord>>> {
    let mut text = String::new();
    let mut file = match File::open(marker) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    file.read_to_string(&mut text)?;
    Ok(Some(
        text.lines()
            .filter_map(|line| {
                if let Some(logical) = line.strip_prefix("dir\t") {
                    return Some(OwnershipRecord::Directory(safe_record_path(logical)?));
                }
                let record = line.strip_prefix("file\t").unwrap_or(line);
                let (logical, expected) = record.split_once('\t')?;
                Some(OwnershipRecord::File {
                    logical: safe_record_path(logical)?,
                    expected: expected.to_owned(),
                })
            })
            .collect(),
    ))
}

#[cfg(unix)]
fn record_matches(target: &Path, expected: &str) -> bool {
    fs::symlink_metadata(target)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .and_then(|_| fs::read(target).ok())
        .is_some_and(|bytes| format!("{:016x}", hash_bytes(&bytes)) == expected)
}

#[cfg(unix)]
fn install_items(manifest: &Manifest, options: &InstallOptions, items: &[Item]) -> io::Result<()> {
    let marker = marker_path(manifest, &options.destdir);
    let prior_records = read_records(&marker)?.unwrap_or_default();
    let mut prior_files = Vec::new();
    let mut owned_dirs = BTreeSet::new();
    for record in prior_records {
        match record {
            OwnershipRecord::File { logical, expected } => {
                prior_files.push((logical, expected));
            }
            OwnershipRecord::Directory(logical) => {
                let target = staged(&options.destdir, &logical);
                if fs::symlink_metadata(target).is_ok_and(|metadata| metadata.is_dir()) {
                    owned_dirs.insert(logical);
                }
            }
        }
    }

    let mut records = Vec::new();
    for item in items {
        ensure_parent_dirs(&options.destdir, &item.destination, &mut owned_dirs)?;
        let target = staged(&options.destdir, &item.destination);
        let bytes = source_bytes(item, manifest)?;
        atomic_write(&target, &bytes, item.mode)?;
        let expected = format!("{:016x}", hash_bytes(&bytes));
        records.push(format!("file\t{}\t{expected}", item.destination.display()));
    }
    for (logical, expected) in prior_files {
        if items.iter().any(|item| item.destination == logical) {
            continue;
        }
        let target = staged(&options.destdir, &logical);
        if record_matches(&target, &expected) {
            fs::remove_file(target)?;
        }
    }

    ensure_parent_dirs(
        &options.destdir,
        &manifest.dirs.sysconfdir.join("wiiland/.xtask-install"),
        &mut owned_dirs,
    )?;
    records.extend(
        owned_dirs
            .into_iter()
            .map(|logical| format!("dir\t{}", logical.display())),
    );
    records.sort();
    atomic_write(
        &marker,
        format!("{}\n", records.join("\n")).as_bytes(),
        0o644,
    )
}

#[cfg(unix)]
fn safe_record_path(value: &str) -> Option<PathBuf> {
    let path = PathBuf::from(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
    {
        return None;
    }
    Some(path)
}

pub fn uninstall(manifest: &Manifest, destdir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let marker = marker_path(manifest, destdir);
        let Some(records) = read_records(&marker)? else {
            return Ok(());
        };
        let mut owned_dirs = Vec::new();
        for record in records {
            match record {
                OwnershipRecord::File { logical, expected } => {
                    let target = staged(destdir, &logical);
                    if record_matches(&target, &expected) {
                        fs::remove_file(target)?;
                    }
                }
                OwnershipRecord::Directory(logical) => owned_dirs.push(logical),
            }
        }
        fs::remove_file(&marker)?;
        owned_dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
        owned_dirs.dedup();
        for logical in owned_dirs {
            let _ = fs::remove_dir(staged(destdir, &logical));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (manifest, destdir);
        Err(unsupported_unix_packaging())
    }
}
#[cfg(not(unix))]
fn unsupported_unix_packaging() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix install and uninstall are unsupported on this platform; use packaging/windows for the Windows installer",
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::manifest::{Features, LogicalDirOverrides};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "wiiland-xtask-{name}-{}-{}",
                process::id(),
                NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn manifest(root: &Path) -> Manifest {
        let dirs = LogicalDirOverrides {
            prefix: Some(PathBuf::from("/opt/wiiland")),
            ..LogicalDirOverrides::default()
        }
        .resolve();
        Manifest {
            root: root.to_path_buf(),
            dirs,
            features: Features::default(),
            udev_dir: None,
            systemd_dir: None,
            xorg_dir: None,
        }
    }

    fn options(destdir: &Path) -> InstallOptions {
        InstallOptions {
            destdir: destdir.to_path_buf(),
            profile: "test".to_owned(),
        }
    }

    #[test]
    fn generated_service_uses_only_logical_paths() {
        let temp = TestDir::new("generated");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let service_destination = PathBuf::from("/opt/wiiland/lib/systemd/user/wiilandd.service");
        let items = [Item {
            source: ItemSource::GeneratedService,
            destination: service_destination.clone(),
            mode: 0o644,
        }];

        install_items(&manifest, &options, &items).unwrap();

        let service = fs::read_to_string(staged(&options.destdir, &service_destination)).unwrap();
        assert_eq!(
            service,
            "[Unit]\n\
             Description=WiiLand input bridge\n\
             Documentation=man:wiilandd(1)\n\
             \n\
             [Service]\n\
             Type=simple\n\
             NoNewPrivileges=yes\n\
             LockPersonality=yes\n\
             MemoryDenyWriteExecute=yes\n\
             RestrictRealtime=yes\n\
             RestrictSUIDSGID=yes\n\
             SystemCallArchitectures=native\n\
             UMask=0077\n\
             ExecCondition=/opt/wiiland/bin/wiilandd --check-config\n\
             ExecStart=/opt/wiiland/bin/wiilandd\n\
             Restart=on-failure\n\
             RestartSec=2s\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
        let destdir = options.destdir.to_string_lossy();
        assert!(!service.contains(destdir.as_ref()));
    }

    #[test]
    fn config_install_uses_canonical_runtime_path_under_destdir() {
        let temp = TestDir::new("config-path");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let source = temp.path().join("wiilandd.conf");
        fs::write(&source, b"[device]\n").unwrap();
        let destination = manifest.dirs.sysconfdir.join("wiiland/wiilandd.conf");

        install_items(
            &manifest,
            &options,
            &[Item {
                source: ItemSource::Root(source),
                destination: destination.clone(),
                mode: 0o644,
            }],
        )
        .unwrap();

        assert_eq!(destination, Path::new("/etc/wiiland/wiilandd.conf"));
        assert_eq!(
            fs::read(staged(&options.destdir, &destination)).unwrap(),
            b"[device]\n"
        );
        assert!(
            !options
                .destdir
                .join("opt/wiiland/etc/wiiland/wiilandd.conf")
                .exists()
        );
    }

    #[test]
    fn safe_record_path_rejects_unstaged_and_traversing_paths() {
        assert_eq!(
            safe_record_path("/opt/wiiland/bin/wiilandd"),
            Some(PathBuf::from("/opt/wiiland/bin/wiilandd"))
        );
        assert_eq!(safe_record_path("opt/wiiland/bin/wiilandd"), None);
        assert_eq!(safe_record_path("../opt/wiiland/bin/wiilandd"), None);
        assert_eq!(safe_record_path("/opt/wiiland/../foreign"), None);
        assert_eq!(safe_record_path(""), None);
    }

    #[test]
    fn install_marker_records_content_hashes() {
        let temp = TestDir::new("marker");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let source = temp.path().join("source");
        let contents = b"installed contents\n";
        fs::write(&source, contents).unwrap();
        let file_destination = PathBuf::from("/opt/wiiland/bin/wiilandd");
        let items = [Item {
            source: ItemSource::Root(source),
            destination: file_destination,
            mode: 0o755,
        }];

        install_items(&manifest, &options, &items).unwrap();

        assert_eq!(
            fs::read_to_string(marker_path(&manifest, &options.destdir)).unwrap(),
            format!(
                "dir\t/\n\
                 dir\t/etc\n\
                 dir\t/etc/wiiland\n\
                 dir\t/opt\n\
                 dir\t/opt/wiiland\n\
                 dir\t/opt/wiiland/bin\n\
                 file\t/opt/wiiland/bin/wiilandd\t{:016x}\n",
                hash_bytes(contents)
            )
        );
    }

    #[test]
    fn repeated_install_removes_deselected_unchanged_owned_files() {
        let temp = TestDir::new("repeat-stale-owned");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let selected_source = temp.path().join("selected-source");
        let stale_source = temp.path().join("stale-source");
        fs::write(&selected_source, b"selected\n").unwrap();
        fs::write(&stale_source, b"stale\n").unwrap();
        let selected_destination = PathBuf::from("/opt/wiiland/bin/selected");
        let stale_destination = PathBuf::from("/opt/wiiland/bin/stale");
        let selected = Item {
            source: ItemSource::Root(selected_source),
            destination: selected_destination.clone(),
            mode: 0o755,
        };
        let stale = Item {
            source: ItemSource::Root(stale_source),
            destination: stale_destination.clone(),
            mode: 0o755,
        };

        install_items(&manifest, &options, &[selected.clone(), stale]).unwrap();
        install_items(&manifest, &options, &[selected]).unwrap();

        assert!(!staged(&options.destdir, &stale_destination).exists());
        let marker = fs::read_to_string(marker_path(&manifest, &options.destdir)).unwrap();
        assert!(!marker.contains(stale_destination.to_string_lossy().as_ref()));
        uninstall(&manifest, &options.destdir).unwrap();
        assert!(!staged(&options.destdir, &selected_destination).exists());
    }

    #[test]
    fn repeated_install_retains_created_directory_ownership_for_uninstall() {
        let temp = TestDir::new("repeat-created-dirs");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let source = temp.path().join("source");
        fs::write(&source, b"installed\n").unwrap();
        let item = Item {
            source: ItemSource::Root(source),
            destination: PathBuf::from("/opt/wiiland/bin/owned"),
            mode: 0o755,
        };

        install_items(&manifest, &options, std::slice::from_ref(&item)).unwrap();
        install_items(&manifest, &options, &[item]).unwrap();
        uninstall(&manifest, &options.destdir).unwrap();

        assert!(!options.destdir.exists());
    }

    #[test]
    fn uninstall_never_removes_preexisting_empty_directories() {
        let temp = TestDir::new("preexisting-dirs");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let destination = PathBuf::from("/opt/wiiland/bin/owned");
        let preexisting = staged(&options.destdir, destination.parent().unwrap());
        fs::create_dir_all(&preexisting).unwrap();
        let source = temp.path().join("source");
        fs::write(&source, b"installed\n").unwrap();

        install_items(
            &manifest,
            &options,
            &[Item {
                source: ItemSource::Root(source),
                destination,
                mode: 0o755,
            }],
        )
        .unwrap();
        uninstall(&manifest, &options.destdir).unwrap();

        assert!(preexisting.is_dir());
    }

    #[test]
    fn repeated_install_leaves_modified_stale_and_foreign_files_unclaimed() {
        let temp = TestDir::new("repeat-modified");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let stale_source = temp.path().join("stale-source");
        fs::write(&stale_source, b"original\n").unwrap();
        let stale_destination = PathBuf::from("/opt/wiiland/bin/stale");
        let stale_target = staged(&options.destdir, &stale_destination);
        let foreign_target = staged(&options.destdir, Path::new("/opt/wiiland/bin/foreign"));

        install_items(
            &manifest,
            &options,
            &[Item {
                source: ItemSource::Root(stale_source),
                destination: stale_destination,
                mode: 0o755,
            }],
        )
        .unwrap();
        fs::write(&stale_target, b"locally modified\n").unwrap();
        fs::write(&foreign_target, b"foreign\n").unwrap();

        install_items(&manifest, &options, &[]).unwrap();
        uninstall(&manifest, &options.destdir).unwrap();

        assert_eq!(fs::read(stale_target).unwrap(), b"locally modified\n");
        assert_eq!(fs::read(foreign_target).unwrap(), b"foreign\n");
    }

    #[test]
    fn uninstall_removes_only_unchanged_owned_entries() {
        let temp = TestDir::new("uninstall");
        let manifest = manifest(temp.path());
        let options = options(&temp.path().join("stage"));
        let owned_source = temp.path().join("owned-source");
        let modified_source = temp.path().join("modified-source");
        fs::write(&owned_source, b"owned\n").unwrap();
        fs::write(&modified_source, b"original\n").unwrap();
        let owned_destination = PathBuf::from("/opt/wiiland/bin/owned");
        let modified_destination = PathBuf::from("/opt/wiiland/bin/modified");
        let items = [
            Item {
                source: ItemSource::Root(owned_source),
                destination: owned_destination.clone(),
                mode: 0o755,
            },
            Item {
                source: ItemSource::Root(modified_source),
                destination: modified_destination.clone(),
                mode: 0o644,
            },
        ];
        install_items(&manifest, &options, &items).unwrap();
        let owned = staged(&options.destdir, &owned_destination);
        let modified = staged(&options.destdir, &modified_destination);
        let sentinel = staged(
            &options.destdir,
            Path::new("/opt/wiiland/bin/foreign-sentinel"),
        );
        fs::write(&modified, b"locally modified\n").unwrap();
        fs::write(&sentinel, b"foreign\n").unwrap();

        uninstall(&manifest, &options.destdir).unwrap();

        assert_eq!(
            fs::symlink_metadata(&owned).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs::read(&modified).unwrap(), b"locally modified\n");
        assert_eq!(fs::read(&sentinel).unwrap(), b"foreign\n");
        assert_eq!(
            fs::symlink_metadata(marker_path(&manifest, &options.destdir))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
