use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use tempfile::TempDir;

static PROCESS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[allow(dead_code)]
enum EnvChange {
    Set(&'static str, OsString),
    Unset(&'static str),
    PrependPath(PathBuf),
}

// Serializes process-global test mutations and restores cwd, env, PATH,
// fake codex binaries, and runtime permit-pool paths after each sandboxed run.
pub struct ProcessSandbox {
    root: TempDir,
    cwd: Option<PathBuf>,
    env: Vec<EnvChange>,
}

#[allow(dead_code)]
pub struct ProcessSandboxGuard {
    original_cwd: PathBuf,
    originals: Vec<(&'static str, Option<OsString>)>,
}

#[allow(dead_code)]
impl ProcessSandbox {
    pub fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
            cwd: None,
            env: Vec::new(),
        }
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn temp_path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    pub fn enter_cwd(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.cwd = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn set_env(&mut self, key: &'static str, value: impl Into<OsString>) -> &mut Self {
        self.env.push(EnvChange::Set(key, value.into()));
        self
    }

    pub fn unset_env(&mut self, key: &'static str) -> &mut Self {
        self.env.push(EnvChange::Unset(key));
        self
    }

    pub fn prepend_path(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.env
            .push(EnvChange::PrependPath(path.as_ref().to_path_buf()));
        self
    }

    pub fn runtime_root(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.set_env(
            fkst_common::runtime_layout::RUNTIME_ROOT_ENV,
            path.as_ref().as_os_str().to_owned(),
        )
    }

    pub fn runtime_log_dir(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.set_env("FKST_RUNTIME_LOG_DIR", path.as_ref().as_os_str().to_owned())
    }

    pub fn durable_root(&mut self, path: impl AsRef<Path>) -> &mut Self {
        self.set_env(
            fkst_common::durable_layout::DURABLE_ROOT_ENV,
            path.as_ref().as_os_str().to_owned(),
        )
    }

    pub fn enter(&self) -> (std::sync::MutexGuard<'static, ()>, ProcessSandboxGuard) {
        let lock = PROCESS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let original_cwd = std::env::current_dir().unwrap();
        let originals = self
            .env
            .iter()
            .map(|change| {
                let key = match change {
                    EnvChange::Set(key, _) | EnvChange::Unset(key) => *key,
                    EnvChange::PrependPath(_) => "PATH",
                };
                (key, std::env::var_os(key))
            })
            .collect::<Vec<_>>();

        for change in &self.env {
            match change {
                EnvChange::Set(key, value) => std::env::set_var(key, value),
                EnvChange::Unset(key) => std::env::remove_var(key),
                EnvChange::PrependPath(path) => {
                    let old_path = std::env::var_os("PATH").unwrap_or_default();
                    let mut value = OsString::from(path);
                    value.push(OsStr::new(":"));
                    value.push(old_path);
                    std::env::set_var("PATH", value);
                }
            }
        }
        if let Some(cwd) = &self.cwd {
            std::env::set_current_dir(cwd).unwrap();
        }

        (
            lock,
            ProcessSandboxGuard {
                original_cwd,
                originals,
            },
        )
    }

    pub fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let (_lock, guard) = self.enter();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        drop(guard);

        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
}

impl Drop for ProcessSandboxGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
        for (key, value) in self.originals.drain(..).rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
