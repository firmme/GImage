use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub struct TaskRecord {
    pub title: String,
    pub total: usize,
}

pub struct TaskJournal {
    session: PathBuf,
    _owner: File,
}

impl TaskJournal {
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let session = root.join(format!("{}-{}", std::process::id(), started.as_nanos()));
        fs::create_dir(&session)?;
        let owner = File::create_new(session.join("owner.lock"))?;
        owner.lock()?;
        Ok(Self {
            session,
            _owner: owner,
        })
    }

    /// A batch may change files only after its record has reached disk.
    pub fn start(&self, id: u64, record: &TaskRecord) -> io::Result<()> {
        let temp = self.session.join(format!("{id}.tmp"));
        let result = (|| {
            let mut file = File::create_new(&temp)?;
            serde_json::to_writer(&mut file, record)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, self.record_path(id))
        })();
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }

    pub fn finish(&self, id: u64) -> io::Result<()> {
        Self::clear(&self.record_path(id))
    }

    fn record_path(&self, id: u64) -> PathBuf {
        self.session.join(format!("{id}.json"))
    }

    pub fn clear(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    pub fn interrupted(&self) -> io::Result<Vec<(PathBuf, TaskRecord)>> {
        let mut interrupted = Vec::new();
        let root = self.session.parent().expect("journal session has a parent");
        for session in fs::read_dir(root)? {
            let session = session?.path();
            if session == self.session {
                continue;
            }
            match Self::read_session(&session, &mut interrupted) {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(%error, path = %session.display(), "could not read task session")
                }
            }
        }
        Ok(interrupted)
    }

    fn read_session(
        session: &Path,
        interrupted: &mut Vec<(PathBuf, TaskRecord)>,
    ) -> io::Result<()> {
        let owner = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(session.join("owner.lock"))
        {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            result => result?,
        };
        match owner.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(()),
            Err(TryLockError::Error(error)) => return Err(error),
        }
        for entry in fs::read_dir(session)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match File::open(&path).and_then(|file| Ok(serde_json::from_reader(file)?)) {
                    Ok(record) => interrupted.push((path, record)),
                    Err(error) => {
                        tracing::warn!(%error, path = %path.display(), "could not read task record")
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const CHILD_ROOT: &str = "GIMAGE_TEST_TASK_JOURNAL_CHILD";

    #[test]
    fn journal_child() {
        let Some(root) = std::env::var_os(CHILD_ROOT) else {
            return;
        };
        let root = PathBuf::from(root);
        let journal = TaskJournal::open(&root).unwrap();
        let record = TaskRecord {
            title: "批量导出".into(),
            total: 20,
        };
        journal.start(1, &record).unwrap();
        journal.start(2, &record).unwrap();
        journal.finish(2).unwrap();
        fs::write(root.join("ready"), []).unwrap();
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn detects_abandoned_tasks_after_process_termination() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let journal = TaskJournal::open(root).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "task_journal::tests::journal_child"])
            .env(CHILD_ROOT, root)
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !root.join("ready").exists() && Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let ready = root.join("ready").exists();
        let while_live = journal.interrupted();
        let killed = child.kill();
        let exit = child.wait().unwrap();

        assert!(ready, "journal child did not start");
        killed.unwrap();
        assert!(!exit.success());
        assert!(while_live.unwrap().is_empty());
        let recovered = journal.interrupted().unwrap();
        let [(path, record)] = recovered.as_slice() else {
            panic!("expected one interrupted task, found {}", recovered.len());
        };
        assert_eq!(record.title, "批量导出");
        assert_eq!(record.total, 20);
        assert!(path.ends_with("1.json"));
        TaskJournal::clear(path).unwrap();
        assert!(journal.interrupted().unwrap().is_empty());
    }

    #[test]
    fn unusable_journal_path_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal");
        fs::write(&path, b"occupied").unwrap();
        assert!(TaskJournal::open(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"occupied");
    }
}
