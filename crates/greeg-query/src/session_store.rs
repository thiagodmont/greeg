//! Private, bounded session logs. All leaf operations use an anchored directory.
use crate::session::{MAX_AGE_SECS, MAX_RECORDS, Record};
use greeg_index::private::{Lock, PrivateDir};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub(crate) const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 256 * 1024;

pub(crate) struct Store {
    dir: PrivateDir,
    #[cfg(test)]
    path: PathBuf,
    name: String,
}

pub(crate) struct Loaded {
    pub records: Vec<Record>,
    bytes: usize,
    compact: bool,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

impl Store {
    pub fn open(path: &Path, key: &str) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("session directory has no parent"))?;
        Ok(Self {
            dir: PrivateDir::open(parent, "session")?,
            #[cfg(test)]
            path: path.to_owned(),
            name: format!("{key}.jsonl"),
        })
    }

    fn file(&self, name: &str, flags: i32) -> io::Result<File> {
        self.dir.file(name, flags)
    }

    pub fn load(&self, now: u64) -> io::Result<Loaded> {
        let file = match self.file(&self.name, libc::O_RDONLY) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Loaded {
                    records: Vec::new(),
                    bytes: 0,
                    compact: false,
                });
            }
            Err(e) => return Err(e),
        };
        if file.metadata()?.len() > MAX_BYTES as u64 {
            return Ok(Loaded {
                records: Vec::new(),
                bytes: 0,
                compact: true,
            });
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_BYTES {
            return Ok(Loaded {
                records: Vec::new(),
                bytes: 0,
                compact: true,
            });
        }
        let mut compact = !bytes.is_empty() && !bytes.ends_with(b"\n");
        let mut records = VecDeque::new();
        for line in bytes.split_inclusive(|b| *b == b'\n') {
            if line.len() > MAX_RECORD_BYTES || !line.ends_with(b"\n") {
                compact = true;
                continue;
            }
            match serde_json::from_slice::<Record>(line) {
                Ok(r) if r.t <= now && now - r.t < MAX_AGE_SECS => {
                    records.push_back(r);
                    if records.len() > MAX_RECORDS {
                        records.pop_front();
                        compact = true;
                    }
                }
                _ => compact = true,
            }
        }
        Ok(Loaded {
            records: records.into(),
            bytes: bytes.len(),
            compact,
        })
    }

    fn lock(&self) -> io::Result<Lock> {
        self.dir.lock(".lock", Duration::from_millis(50))
    }

    pub fn append(&self, record: &Record, now: u64) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        if line.len() > MAX_RECORD_BYTES {
            return Err(invalid("session record exceeds byte limit"));
        }
        let lock = self
            .lock()
            .map_err(|e| io::Error::new(e.kind(), format!("lock: {e}")))?;
        let loaded = self
            .load(now)
            .map_err(|e| io::Error::new(e.kind(), format!("load: {e}")))?;
        if loaded.compact
            || loaded.records.len() >= MAX_RECORDS
            || loaded.bytes + line.len() > MAX_BYTES
        {
            let mut rows = VecDeque::new();
            let mut size = line.len();
            let keep = if loaded.records.len() >= MAX_RECORDS {
                MAX_RECORDS / 2
            } else {
                MAX_RECORDS - 1
            };
            let byte_target = if loaded.bytes + line.len() > MAX_BYTES {
                MAX_BYTES / 2
            } else {
                MAX_BYTES
            };
            for r in loaded.records.iter().rev().take(keep) {
                let mut row = serde_json::to_vec(r)?;
                row.push(b'\n');
                if size + row.len() > byte_target {
                    break;
                }
                size += row.len();
                rows.push_front(row);
            }
            let mut body = Vec::with_capacity(size);
            for row in rows {
                body.extend(row);
            }
            body.extend(line);
            self.replace(&body)
                .map_err(|e| io::Error::new(e.kind(), format!("replace: {e}")))?;
        } else {
            let mut file =
                self.file(&self.name, libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND)?;
            file.write_all(&line)?;
        }
        drop(lock);
        if loaded.bytes == 0 {
            self.prune();
        }
        Ok(())
    }

    fn replace(&self, body: &[u8]) -> io::Result<()> {
        self.dir.replace(&self.name, body)
    }

    fn expired_log(&self, name: &str) -> bool {
        let Ok(file) = self.dir.open_unchecked(name, libc::O_RDONLY) else {
            return false;
        };
        let Ok(m) = file.metadata() else { return false };
        // SAFETY: geteuid has no preconditions.
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age.as_secs() >= MAX_AGE_SECS)
    }

    fn prune(&self) {
        let mut expired = Vec::new();
        for name in self.dir.names(4096) {
            let Some(key) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let safe = !key.is_empty()
                && key.len() <= 64
                && key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_');
            let hashed = key.strip_prefix("h-").is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            });
            if name == self.name || !(safe || hashed) {
                continue;
            }
            if self.expired_log(&name) {
                expired.push(name);
            }
        }
        for name in expired {
            let Some(_lock) = self.dir.try_lock(".lock") else {
                return;
            };
            // A writer may have refreshed or replaced the log during enumeration.
            if self.expired_log(&name) {
                let _ = self.dir.unlink(&name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = loop {
                let candidate = std::env::temp_dir().join(format!(
                    "greeg-session-store-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&candidate) {
                    Ok(()) => break candidate,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("fixture directory: {e}"),
                }
            };
            Self(path)
        }
        fn store(&self, key: &str) -> Store {
            Store::open(&self.0.join("session"), key).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn record(n: usize) -> Record {
        Record {
            t: 100_000,
            q: n.to_string(),
            files: vec![format!("{n}.rs")],
            ..Record::default()
        }
    }
    fn seed(store: &Store, count: usize) {
        let bytes: Vec<_> = (0..count)
            .flat_map(|n| {
                let mut b = serde_json::to_vec(&record(n)).unwrap();
                b.push(b'\n');
                b
            })
            .collect();
        fs::write(store.path.join(&store.name), bytes).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extended_acl_is_refused_without_changing_permissions() {
        let f = Fixture::new();
        let store = f.store("acl");
        seed(&store, 1);
        let path = store.path.join(&store.name);
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read,write"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let before = fs::metadata(&path).unwrap().mode();
        assert!(store.load(100_000).is_err());
        assert!(store.append(&record(2), 100_000).is_err());
        assert_eq!(fs::metadata(&path).unwrap().mode(), before);
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow list,search,add_file"])
            .arg(&store.path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(Store::open(&store.path, "acl").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn posix_acl_is_refused_without_changing_permissions() {
        use std::os::fd::AsRawFd;
        let f = Fixture::new();
        let store = f.store("acl");
        seed(&store, 1);
        let path = store.path.join(&store.name);
        let file = File::open(&path).unwrap();
        let mut acl = 2u32.to_le_bytes().to_vec();
        // Linux POSIX ACL xattr: version, then (tag, permissions, qualifier).
        for (tag, perm, id) in [
            (1u16, 6u16, u32::MAX),
            (2, 4, file.metadata().unwrap().uid() + 1),
            (4, 0, u32::MAX),
            (16, 4, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend(tag.to_le_bytes());
            acl.extend(perm.to_le_bytes());
            acl.extend(id.to_le_bytes());
        }
        // SAFETY: valid descriptor, ACL name and byte buffer of the supplied length.
        assert_eq!(
            unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    c"system.posix_acl_access".as_ptr(),
                    acl.as_ptr().cast(),
                    acl.len(),
                    0,
                )
            },
            0
        );
        let before = file.metadata().unwrap().mode();
        assert!(store.load(100_000).is_err());
        assert!(store.append(&record(2), 100_000).is_err());
        assert_eq!(file.metadata().unwrap().mode(), before);
    }

    #[test]
    fn publication_stays_in_the_opened_directory_after_a_rename() {
        let f = Fixture::new();
        let store = f.store("anchored");
        seed(&store, MAX_RECORDS);
        let moved = f.0.join("moved");
        fs::rename(&store.path, &moved).unwrap();
        fs::create_dir(&store.path).unwrap();
        fs::write(store.path.join(&store.name), "untouched").unwrap();
        store.append(&record(MAX_RECORDS), 100_000).unwrap();
        assert_eq!(
            fs::read_to_string(store.path.join(&store.name)).unwrap(),
            "untouched"
        );
        assert_eq!(
            fs::read_to_string(moved.join(&store.name))
                .unwrap()
                .lines()
                .count(),
            MAX_RECORDS / 2 + 1
        );
    }

    #[test]
    fn pruning_stays_anchored_and_restarts_each_sweep() {
        let f = Fixture::new();
        let store = f.store("active");
        let moved = f.0.join("moved");
        fs::rename(&store.path, &moved).unwrap();
        fs::create_dir(&store.path).unwrap();
        let old = SystemTime::now() - Duration::from_secs(MAX_AGE_SECS + 1);
        for name in ["first.jsonl", "second.jsonl"] {
            let path = moved.join(name);
            fs::write(&path, "expired").unwrap();
            File::open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
            fs::write(store.path.join("unrelated.jsonl"), "untouched").unwrap();
            store.prune();
            assert!(!path.exists());
            assert_eq!(
                fs::read_to_string(store.path.join("unrelated.jsonl")).unwrap(),
                "untouched"
            );
        }
    }

    #[test]
    fn pruning_skips_busy_writers_then_rechecks_expiry() {
        let f = Fixture::new();
        let store = f.store("active");
        let other = f.store("other");
        seed(&other, 1);
        let path = other.path.join(&other.name);
        File::open(&path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(MAX_AGE_SECS + 1)),
            )
            .unwrap();
        let lock = other.lock().unwrap();
        store.prune();
        assert!(path.exists(), "cleanup must not race a writer");
        drop(lock);
        other.append(&record(2), 100_000).unwrap();
        store.prune();
        assert_eq!(other.load(100_000).unwrap().records.len(), 2);
    }

    #[test]
    fn private_modes_and_parent_permissions() {
        let f = Fixture::new();
        fs::set_permissions(&f.0, fs::Permissions::from_mode(0o755)).unwrap();
        let store = f.store("privacy");
        seed(&store, 1);
        fs::set_permissions(
            store.path.join(&store.name),
            fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        store.append(&record(1), 100_000).unwrap();
        for (path, mode) in [
            (&f.0, 0o755),
            (&store.path, 0o700),
            (&store.path.join(&store.name), 0o600),
            (&store.path.join(".lock"), 0o600),
        ] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o7777, mode);
        }
        assert!(
            !fs::read_to_string(store.path.join(&store.name))
                .unwrap()
                .contains("\"pat\"")
        );
    }

    #[test]
    fn unsafe_objects_are_not_read_or_written() {
        for kind in [
            "file_symlink",
            "directory_symlink",
            "hardlink",
            "lock_symlink",
            "fifo",
        ] {
            let f = Fixture::new();
            let outside = f.0.join("outside");
            fs::write(&outside, "untouched").unwrap();
            let original_mode = fs::metadata(&outside).unwrap().mode();
            if kind == "directory_symlink" {
                symlink(&f.0, f.0.join("session")).unwrap();
                assert!(Store::open(&f.0.join("session"), "test").is_err());
            } else {
                let store = f.store("test");
                let path = store.path.join(&store.name);
                match kind {
                    "file_symlink" => symlink(&outside, &path).unwrap(),
                    "hardlink" => fs::hard_link(&outside, &path).unwrap(),
                    "lock_symlink" => symlink(&outside, store.path.join(".lock")).unwrap(),
                    "fifo" => {
                        use std::os::unix::ffi::OsStrExt;
                        let p = CString::new(path.as_os_str().as_bytes()).unwrap();
                        // SAFETY: valid NUL-terminated fixture path.
                        assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o600) }, 0);
                    }
                    _ => unreachable!(),
                }
                assert!(store.append(&record(0), 100_000).is_err(), "{kind}");
                if kind != "lock_symlink" {
                    assert!(store.load(100_000).is_err(), "{kind}");
                }
            }
            assert_eq!(fs::read_to_string(&outside).unwrap(), "untouched", "{kind}");
            assert_eq!(
                fs::metadata(&outside).unwrap().mode(),
                original_mode,
                "{kind}"
            );
        }
    }

    #[test]
    fn bounded_load_and_atomic_compaction_use_latest_records() {
        let f = Fixture::new();
        let first = f.store("shared");
        seed(&first, MAX_RECORDS);
        let stale = f.store("shared");
        assert_eq!(stale.load(100_000).unwrap().records.len(), MAX_RECORDS);
        let old = first.file(&first.name, libc::O_RDONLY).unwrap();
        first.append(&record(MAX_RECORDS), 100_000).unwrap();
        stale.append(&record(MAX_RECORDS + 1), 100_000).unwrap();
        let loaded = first.load(100_000).unwrap();
        assert_eq!(loaded.records.len(), MAX_RECORDS / 2 + 2);
        assert_eq!(loaded.records[0].q, (MAX_RECORDS / 2).to_string());
        assert_eq!(
            loaded.records.last().unwrap().q,
            (MAX_RECORDS + 1).to_string()
        );
        let mut original = String::new();
        (&old).read_to_string(&mut original).unwrap();
        assert_eq!(original.lines().count(), MAX_RECORDS);
        assert!(
            original
                .lines()
                .last()
                .unwrap()
                .contains(&format!("\"q\":\"{}\"", MAX_RECORDS - 1))
        );
        let large = first.file(&first.name, libc::O_WRONLY).unwrap();
        large.set_len(MAX_BYTES as u64 + 1).unwrap();
        assert!(first.load(100_000).unwrap().records.is_empty());
        first.append(&record(9), 100_000).unwrap();
        assert_eq!(first.load(100_000).unwrap().records.len(), 1);
        assert!(fs::metadata(first.path.join(&first.name)).unwrap().len() < MAX_BYTES as u64);
    }

    #[test]
    fn retention_discards_expired_future_malformed_and_partial_records() {
        let f = Fixture::new();
        let store = f.store("age");
        let mut expired = record(1);
        expired.t = 100_000 - MAX_AGE_SECS;
        let mut future = record(2);
        future.t += 1;
        let text = format!(
            "{}\n{}\n{}\ngarbage\n{}",
            serde_json::to_string(&expired).unwrap(),
            serde_json::to_string(&future).unwrap(),
            serde_json::to_string(&record(3)).unwrap(),
            serde_json::to_string(&record(4)).unwrap()
        );
        fs::write(store.path.join(&store.name), text).unwrap();
        assert_eq!(
            store
                .load(100_000)
                .unwrap()
                .records
                .iter()
                .map(|r| r.q.as_str())
                .collect::<Vec<_>>(),
            ["3"]
        );
        store.append(&record(5), 100_000).unwrap();
        let text = fs::read_to_string(store.path.join(&store.name)).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with('\n'));
        let mut oversized = record(6);
        oversized.q = "x".repeat(MAX_RECORD_BYTES);
        assert!(store.append(&oversized, 100_000).is_err());
        assert_eq!(
            fs::read_to_string(store.path.join(&store.name)).unwrap(),
            text
        );
    }

    #[test]
    fn concurrent_writers_compact_without_losing_new_records() {
        let f = Fixture::new();
        seed(&f.store("parallel"), MAX_RECORDS);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|scope| {
            for n in 0..8 {
                let barrier = barrier.clone();
                let f = &f;
                scope.spawn(move || {
                    let store = f.store("parallel");
                    barrier.wait();
                    let deadline = Instant::now() + Duration::from_secs(2);
                    loop {
                        match store.append(&record(MAX_RECORDS + n), 100_000) {
                            Ok(()) => break,
                            Err(e)
                                if e.kind() == io::ErrorKind::WouldBlock
                                    && Instant::now() < deadline =>
                            {
                                continue;
                            }
                            Err(e) => panic!("append: {e}"),
                        }
                    }
                });
            }
        });
        let loaded = f.store("parallel").load(100_000).unwrap();
        assert_eq!(loaded.records.len(), MAX_RECORDS / 2 + 8);
        for n in 0..8 {
            assert!(
                loaded
                    .records
                    .iter()
                    .any(|r| r.q == (MAX_RECORDS + n).to_string())
            );
        }
    }

    #[test]
    fn busy_lock_is_bounded_and_explicitly_released() {
        let f = Fixture::new();
        let store = f.store("locked");
        let lock = store.lock().unwrap();
        let _inherited = lock.file().try_clone().unwrap();
        let start = Instant::now();
        assert!(store.append(&record(0), 100_000).is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(lock);
        store.append(&record(1), 100_000).unwrap();
    }

    #[test]
    fn pruning_removes_a_bulk_batch_and_preserves_fresh_logs() {
        let f = Fixture::new();
        let store = f.store("active");
        let old = SystemTime::now() - Duration::from_secs(MAX_AGE_SECS + 1);
        for n in 0..512 {
            let path = store.path.join(format!("expired-{n}.jsonl"));
            fs::write(&path, "expired").unwrap();
            File::open(path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        fs::write(store.path.join("fresh.jsonl"), "keep").unwrap();
        store.prune();
        for n in 0..512 {
            assert!(!store.path.join(format!("expired-{n}.jsonl")).exists());
        }
        assert_eq!(
            fs::read_to_string(store.path.join("fresh.jsonl")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn prune_only_expired_regular_owned_log_names() {
        let f = Fixture::new();
        let store = f.store("new");
        let old = SystemTime::now() - Duration::from_secs(MAX_AGE_SECS + 1);
        for name in [
            "expired.jsonl",
            "keep.txt",
            ".jsonl",
            "fresh.jsonl",
            "linked.jsonl",
        ] {
            let path = store.path.join(name);
            fs::write(&path, b"data").unwrap();
            if name != "fresh.jsonl" {
                File::open(path)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(old))
                    .unwrap();
            }
        }
        fs::hard_link(store.path.join("linked.jsonl"), f.0.join("alias")).unwrap();
        symlink(
            store.path.join("keep.txt"),
            store.path.join("symlink.jsonl"),
        )
        .unwrap();
        store.append(&record(0), 100_000).unwrap();
        assert!(!store.path.join("expired.jsonl").exists());
        for name in [
            "keep.txt",
            ".jsonl",
            "fresh.jsonl",
            "linked.jsonl",
            "symlink.jsonl",
        ] {
            assert!(store.path.join(name).exists(), "{name}");
        }
    }
}
