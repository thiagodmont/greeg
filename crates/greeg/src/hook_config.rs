use anyhow::{Context, Result, bail, ensure};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Config {
    path: PathBuf,
    text: Option<String>,
    metadata: Option<Metadata>,
    file: Option<File>,
}

fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path: path.to_owned(),
                    text: None,
                    metadata: None,
                    file: None,
                });
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("read {} (regular file required)", path.display()));
            }
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file(),
            "{} is not a regular file",
            path.display()
        );
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("read {}", path.display()))?;
        ensure!(
            unchanged(&metadata, &file.metadata()?),
            "configuration changed while reading {}; retry",
            path.display()
        );
        Ok(Self {
            path: path.to_owned(),
            text: Some(text),
            metadata: Some(metadata),
            file: Some(file),
        })
    }

    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    pub fn write(&self, text: &str) -> Result<()> {
        if text == self.text().unwrap_or("") {
            return Ok(());
        }
        self.prepare(text)?.commit()
    }

    fn prepare(&self, text: &str) -> Result<Pending<'_>> {
        let parent = self.path.parent().context("configuration has no parent")?;
        fs::create_dir_all(parent)?;
        let name = self
            .path
            .file_name()
            .context("configuration has no filename")?;
        let mut lock_name = std::ffi::OsString::from(".greeg-");
        lock_name.push(name);
        lock_name.push(".lock");
        let lock_path = parent.join(lock_name);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&lock_path)
            .with_context(|| format!("open configuration lock {}", lock_path.display()))?;
        let metadata = lock.metadata()?;
        // SAFETY: geteuid has no arguments or memory access requirements.
        let uid = unsafe { libc::geteuid() };
        ensure!(
            metadata.is_file() && metadata.uid() == uid && metadata.nlink() == 1,
            "configuration lock must be an owned regular file with one link"
        );
        lock.try_lock()
            .context("configuration is busy or cannot be locked; retry")?;
        ensure!(
            unchanged(&metadata, &fs::symlink_metadata(&lock_path)?),
            "configuration lock changed; retry"
        );
        self.check_current()?;
        if let Some(metadata) = &self.metadata {
            ensure!(
                metadata.uid() == uid && metadata.nlink() == 1,
                "configuration must be owned by the current user and have one link; left unchanged"
            );
            let writable = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&self.path)
                .context("configuration is not writable; original unchanged")?;
            ensure!(
                unchanged(metadata, &writable.metadata()?),
                "configuration changed; retry"
            );
        }
        for _ in 0..32 {
            let path = parent.join(format!(
                ".greeg-config-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("create temporary configuration"),
            };
            let mut pending = Pending {
                config: self,
                path,
                file,
                _lock: lock,
            };
            pending
                .file
                .write_all(text.as_bytes())
                .context("write temporary configuration; original unchanged")?;
            if let (Some(source), Some(metadata)) = (&self.file, &self.metadata) {
                copy_metadata(source, &pending.file, metadata)?;
            }
            pending
                .file
                .sync_all()
                .context("sync temporary configuration; original unchanged")?;
            return Ok(pending);
        }
        bail!("unable to create an exclusive temporary configuration")
    }

    fn check_current(&self) -> Result<()> {
        let current = Self::read(&self.path)
            .context("configuration changed or cannot be rechecked; retry")?;
        let same_metadata = match (&self.metadata, &current.metadata) {
            (None, None) => true,
            (Some(a), Some(b)) => unchanged(a, b),
            _ => false,
        };
        ensure!(
            same_metadata && self.text == current.text,
            "configuration changed since it was read: {}; left unchanged; retry",
            self.path.display()
        );
        Ok(())
    }
}

struct Pending<'a> {
    config: &'a Config,
    path: PathBuf,
    file: File,
    _lock: File,
}

impl Pending<'_> {
    fn commit(self) -> Result<()> {
        self.config.check_current()?;
        if self.config.metadata.is_some() {
            fs::rename(&self.path, &self.config.path)
                .context("replace configuration; original unchanged")?;
        } else {
            // Linking publishes a complete file without replacing a concurrent creation.
            fs::hard_link(&self.path, &self.config.path)
                .context("publish configuration without overwriting another file; retry")?;
            fs::remove_file(&self.path)
                .context("configuration published but temporary link cleanup failed")?;
        }
        File::open(self.config.path.parent().unwrap())
            .and_then(|directory| directory.sync_all())
            .context("configuration replaced but directory sync failed")?;
        Ok(())
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(target_os = "macos")]
fn copy_metadata(source: &File, dest: &File, _metadata: &Metadata) -> Result<()> {
    // SAFETY: both descriptors are live; a null state requests default behavior.
    let result = unsafe {
        libc::fcopyfile(
            source.as_raw_fd(),
            dest.as_raw_fd(),
            std::ptr::null_mut(),
            libc::COPYFILE_METADATA,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("preserve configuration metadata; original unchanged");
    }
    dest.set_modified(std::time::SystemTime::now())
        .context("set updated configuration timestamp; original unchanged")
}

#[cfg(not(target_os = "macos"))]
fn copy_metadata(source: &File, dest: &File, metadata: &Metadata) -> Result<()> {
    for file in [source, dest] {
        // SAFETY: the descriptor is live; a null, zero-length buffer queries size only.
        let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
        if size < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOTSUP) {
                return Err(error).context("inspect configuration metadata");
            }
        }
        ensure!(
            size <= 0,
            "configuration or replacement has extended attributes or ACLs; automatic replacement is unsupported; original unchanged"
        );
    }
    // SAFETY: the destination is a live descriptor; -1 preserves its owner.
    if unsafe { libc::fchown(dest.as_raw_fd(), !0, metadata.gid()) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("preserve configuration group; original unchanged");
    }
    dest.set_permissions(metadata.permissions())
        .context("preserve configuration mode; original unchanged")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "greeg-config-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }
        fn config(&self) -> PathBuf {
            self.0.join("settings.json")
        }
        fn assert_no_temps(&self) {
            assert!(
                !fs::read_dir(&self.0).unwrap().any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp"))
            );
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn stale_snapshots_and_external_edits_are_preserved() {
        for change in ["write", "replace", "remove", "chmod", "symlink"] {
            let f = Fixture::new();
            let path = f.config();
            fs::write(&path, "original").unwrap();
            let config = Config::read(&path).unwrap();
            let pending = config.prepare("replacement").unwrap();
            match change {
                "write" => fs::write(&path, "external").unwrap(),
                "replace" => {
                    let other = f.0.join("other");
                    fs::write(&other, "original").unwrap();
                    fs::rename(other, &path).unwrap();
                }
                "remove" => fs::remove_file(&path).unwrap(),
                "chmod" => fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap(),
                "symlink" => {
                    fs::remove_file(&path).unwrap();
                    fs::write(f.0.join("target"), "external").unwrap();
                    symlink(f.0.join("target"), &path).unwrap();
                }
                _ => unreachable!(),
            }
            let bytes = fs::read(&path).ok();
            let metadata = fs::symlink_metadata(&path).ok();
            assert!(pending.commit().is_err(), "{change}");
            assert_eq!(fs::read(&path).ok(), bytes);
            if let Some(metadata) = metadata {
                assert!(unchanged(&metadata, &fs::symlink_metadata(&path).unwrap()));
            }
            f.assert_no_temps();
        }
    }

    #[test]
    fn concurrent_creation_and_stale_writer_are_rejected() {
        let f = Fixture::new();
        let path = f.config();
        let absent = Config::read(&path).unwrap();
        let pending = absent.prepare("ours").unwrap();
        fs::write(&path, "external").unwrap();
        assert!(pending.commit().is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "external");
        let first = Config::read(&path).unwrap();
        let second = Config::read(&path).unwrap();
        first.write("first").unwrap();
        assert!(second.write("second").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "first");
        f.assert_no_temps();
    }

    #[test]
    fn lock_contention_is_bounded_and_abandoned_updates_clean_up() {
        let f = Fixture::new();
        fs::write(f.config(), "original").unwrap();
        let first = Config::read(&f.config()).unwrap();
        let pending = first.prepare("first").unwrap();
        let second = Config::read(&f.config()).unwrap();
        let error = second.write("second").unwrap_err();
        assert!(error.to_string().contains("busy"));
        assert_eq!(fs::read_to_string(f.config()).unwrap(), "original");
        drop(pending);
        f.assert_no_temps();
        second.write("second").unwrap();
        assert_eq!(fs::read_to_string(f.config()).unwrap(), "second");
    }

    #[test]
    fn symlink_hardlink_and_lock_redirection_are_rejected() {
        let f = Fixture::new();
        let target = f.0.join("target");
        fs::write(&target, "original").unwrap();
        symlink(&target, f.config()).unwrap();
        assert!(Config::read(&f.config()).is_err());
        fs::remove_file(f.config()).unwrap();
        fs::hard_link(&target, f.config()).unwrap();
        assert!(Config::read(&f.config()).unwrap().write("changed").is_err());
        fs::remove_file(f.config()).unwrap();
        fs::write(f.config(), "original").unwrap();
        let lock = f.0.join(".greeg-settings.json.lock");
        fs::remove_file(&lock).unwrap();
        symlink(&target, lock).unwrap();
        assert!(Config::read(&f.config()).unwrap().write("changed").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "original");
        assert_eq!(fs::read_to_string(f.config()).unwrap(), "original");
        f.assert_no_temps();
    }

    #[test]
    fn modes_noops_and_missing_files_are_preserved() {
        let f = Fixture::new();
        let path = f.0.join("missing/settings.json");
        Config::read(&path).unwrap().write("").unwrap();
        assert!(!path.parent().unwrap().exists());
        Config::read(&f.config()).unwrap().write("new").unwrap();
        assert_eq!(fs::metadata(f.config()).unwrap().mode() & 0o777, 0o600);
        fs::set_permissions(f.config(), fs::Permissions::from_mode(0o640)).unwrap();
        File::options()
            .write(true)
            .open(f.config())
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        Config::read(&f.config()).unwrap().write("next").unwrap();
        let metadata = fs::metadata(f.config()).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o640);
        assert_ne!(
            metadata.modified().unwrap(),
            std::time::SystemTime::UNIX_EPOCH
        );
        Config::read(&f.config()).unwrap().write("next").unwrap();
        assert!(unchanged(&metadata, &fs::metadata(f.config()).unwrap()));
        f.assert_no_temps();
    }

    #[test]
    fn prepared_update_child() {
        let Some(path) = std::env::var_os("GREEG_TEST_PENDING_CONFIG") else {
            return;
        };
        let path = PathBuf::from(path);
        let config = Config::read(&path).unwrap();
        let pending = config.prepare("replacement").unwrap();
        fs::write(path.with_extension("ready"), pending.path.to_str().unwrap()).unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn extended_metadata_is_preserved_or_safely_refused() {
        let f = Fixture::new();
        fs::write(f.config(), "original").unwrap();
        let source = File::open(f.config()).unwrap();
        let name = c"user.greeg-test";
        let value = b"retain";
        // SAFETY: live descriptor and correctly sized name/value buffers.
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::fsetxattr(
                source.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        // SAFETY: live descriptor and correctly sized name/value buffers.
        #[cfg(not(target_os = "macos"))]
        let result = unsafe {
            libc::fsetxattr(
                source.as_raw_fd(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        let update = Config::read(&f.config()).unwrap().write("replacement");
        #[cfg(target_os = "macos")]
        {
            update.unwrap();
            let current = File::open(f.config()).unwrap();
            let mut output = [0u8; 6];
            // SAFETY: live descriptor and writable output buffer of the supplied size.
            let count = unsafe {
                libc::fgetxattr(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    output.as_mut_ptr().cast(),
                    output.len(),
                    0,
                    0,
                )
            };
            assert_eq!(count, 6);
            assert_eq!(&output, value);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(
                update
                    .unwrap_err()
                    .to_string()
                    .contains("extended attributes")
            );
            assert_eq!(fs::read_to_string(f.config()).unwrap(), "original");
        }
        f.assert_no_temps();
    }

    #[test]
    fn killed_writer_leaves_original_intact_and_releases_lock() {
        let f = Fixture::new();
        fs::write(f.config(), "original").unwrap();
        fs::set_permissions(f.config(), fs::Permissions::from_mode(0o600)).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "hook_config::tests::prepared_update_child"])
            .env("GREEG_TEST_PENDING_CONFIG", f.config())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready = f.config().with_extension("ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let was_ready = ready.exists();
        let _ = child.kill();
        child.wait().unwrap();
        assert!(was_ready, "child did not prepare configuration");
        assert_eq!(fs::read_to_string(f.config()).unwrap(), "original");
        let orphan = fs::read_to_string(ready).unwrap();
        assert_eq!(fs::metadata(orphan).unwrap().mode() & 0o777, 0o600);
        Config::read(&f.config())
            .unwrap()
            .write("recovered")
            .unwrap();
        assert_eq!(fs::read_to_string(f.config()).unwrap(), "recovered");
    }
}
