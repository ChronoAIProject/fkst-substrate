use mlua::Table;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct DeptRunOptions {
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    path_prepend: Option<String>,
}

impl DeptRunOptions {
    pub(crate) fn default_env() -> Self {
        Self {
            cwd: None,
            env: Vec::new(),
            path_prepend: None,
        }
    }

    pub(crate) fn from_lua(opts: Option<Table>) -> mlua::Result<Self> {
        let Some(opts) = opts else {
            return Ok(Self::default_env());
        };
        let cwd = opts.get::<Option<String>>("cwd")?.map(PathBuf::from);
        let env = match opts.get::<Option<Table>>("env")? {
            Some(env_table) => env_table
                .pairs::<String, String>()
                .collect::<mlua::Result<Vec<(String, String)>>>()?,
            None => Vec::new(),
        };
        let path_prepend = opts.get::<Option<String>>("path_prepend")?;
        Ok(Self {
            cwd,
            env,
            path_prepend,
        })
    }
}

pub(crate) struct DeptRunEnvGuard {
    cwd: Option<PathBuf>,
    env: Vec<(String, Option<std::ffi::OsString>)>,
}

impl DeptRunEnvGuard {
    pub(crate) fn apply(opts: DeptRunOptions) -> mlua::Result<Self> {
        let mut guard = Self {
            cwd: if opts.cwd.is_some() {
                Some(std::env::current_dir().map_err(mlua::Error::external)?)
            } else {
                None
            },
            env: Vec::new(),
        };

        for (key, value) in opts.env {
            guard.env.push((key.clone(), std::env::var_os(&key)));
            std::env::set_var(key, value);
        }

        if let Some(prepend) = opts.path_prepend {
            let key = "PATH".to_string();
            let previous = std::env::var_os(&key);
            let mut paths = vec![PathBuf::from(prepend)];
            if let Some(previous) = &previous {
                paths.extend(std::env::split_paths(previous));
            }
            let next = std::env::join_paths(paths).map_err(mlua::Error::external)?;
            guard.env.push((key.clone(), previous));
            std::env::set_var(key, next);
        }

        if let Some(next_cwd) = opts.cwd {
            if let Err(err) = std::env::set_current_dir(&next_cwd) {
                drop(guard);
                return Err(mlua::Error::external(err));
            }
        }

        Ok(guard)
    }
}

impl Drop for DeptRunEnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.env.iter().rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        if let Some(cwd) = &self.cwd {
            let _ = std::env::set_current_dir(cwd);
        }
    }
}
