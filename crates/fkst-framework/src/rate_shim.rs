//! PATH shims for codex-internal commands that share named rate pools.

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::rate_pool::RatePoolRegistry;

pub(crate) fn ensure_rate_shims(
    registry: &RatePoolRegistry,
    framework_bin: &Path,
) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    ensure_rate_shims_with_path(registry, framework_bin, &path)
}

pub(crate) fn ensure_rate_shims_with_path(
    registry: &RatePoolRegistry,
    framework_bin: &Path,
    path: &OsStr,
) -> Result<PathBuf> {
    let shim_dir = registry.root().join("shims");
    if registry.is_empty() {
        return Ok(shim_dir);
    }
    std::fs::create_dir_all(&shim_dir)
        .with_context(|| format!("create rate shim dir {}", shim_dir.display()))?;
    for name in registry.pools().keys() {
        if resolve_program_on_path(name, Some(path), &shim_dir).is_some() {
            write_shim(&shim_dir, name, framework_bin)?;
        }
    }
    Ok(shim_dir)
}

pub(crate) fn prepend_shim_dir_to_path(cmd: &mut std::process::Command, shim_dir: &Path) {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut path = OsString::from(shim_dir.as_os_str());
    path.push(OsStr::new(":"));
    path.push(current);
    cmd.env("PATH", path);
}

fn write_shim(shim_dir: &Path, name: &str, framework_bin: &Path) -> Result<()> {
    let shim_path = shim_dir.join(name);
    let body = shim_body(name, framework_bin, shim_dir);
    if existing_shim_matches(&shim_path, &body) {
        return Ok(());
    }

    let temp_path = shim_dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        ulid::Ulid::new()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("create {}", temp_path.display()))?;
        file.write_all(body.as_bytes())
            .with_context(|| format!("write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temp_path.display()))?;
    }
    set_executable(&temp_path)?;
    std::fs::rename(&temp_path, &shim_path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), shim_path.display()))?;
    let dir = std::fs::File::open(shim_dir)
        .with_context(|| format!("open rate shim dir {}", shim_dir.display()))?;
    dir.sync_all()
        .with_context(|| format!("sync rate shim dir {}", shim_dir.display()))?;
    Ok(())
}

fn existing_shim_matches(path: &Path, body: &str) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return false;
    };
    if existing != body {
        return false;
    }
    is_executable(path)
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("chmod +x {}", path.display()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn shim_body(name: &str, framework_bin: &Path, shim_dir: &Path) -> String {
    format!(
        "#!/bin/sh\n\
shim_dir={shim_dir}\n\
program={program}\n\
framework_bin={framework_bin}\n\
real_program=\n\
old_ifs=$IFS\n\
IFS=:\n\
for dir in $PATH; do\n\
  if [ -z \"$dir\" ]; then dir=.; fi\n\
  if [ \"$dir\" = \"$shim_dir\" ]; then continue; fi\n\
  candidate=\"$dir/$program\"\n\
  if [ -f \"$candidate\" ] && [ -x \"$candidate\" ]; then\n\
    real_program=$candidate\n\
    break\n\
  fi\n\
done\n\
IFS=$old_ifs\n\
if [ -z \"$real_program\" ]; then\n\
  printf '%s\\n' \"fkst rate shim: real program not found: $program\" >&2\n\
  exit 127\n\
fi\n\
\"$framework_bin\" rate-acquire \"$program\" || exit $?\n\
exec \"$real_program\" \"$@\"\n",
        shim_dir = shell_single_quote(&shim_dir.to_string_lossy()),
        program = shell_single_quote(name),
        framework_bin = shell_single_quote(&framework_bin.to_string_lossy()),
    )
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

pub(crate) fn resolve_program_on_path(
    name: &str,
    path: Option<&OsStr>,
    shim_dir: &Path,
) -> Option<PathBuf> {
    let path = path?;
    for dir in std::env::split_paths(path) {
        if same_path(&dir, shim_dir) {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rate_pool::{RatePoolConfig, RatePoolRegistry};
    use std::collections::BTreeMap;

    #[cfg(unix)]
    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn generated_shim_resolves_real_program_skipping_shim_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let shim_dir = tmp.path().join("shims");
        let real_dir = tmp.path().join("bin");
        let capture = tmp.path().join("capture");
        std::fs::create_dir_all(&shim_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        executable(&real_dir.join("gh"), "#!/bin/sh\nprintf 'real:%s' \"$1\"\n");
        let framework = tmp.path().join("framework");
        executable(
            &framework,
            &format!(
                "#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$2\" > {}\n",
                capture.display()
            ),
        );

        let registry = RatePoolRegistry::for_test(
            tmp.path().to_path_buf(),
            BTreeMap::from([(
                "gh".to_string(),
                RatePoolConfig {
                    burst: 1,
                    refill_per_minute: 1,
                },
            )]),
        );
        let path = std::env::join_paths([shim_dir.as_path(), real_dir.as_path()]).unwrap();
        ensure_rate_shims_with_path(&registry, &framework, &path).unwrap();
        let output = std::process::Command::new(shim_dir.join("gh"))
            .env("PATH", path)
            .arg("ok")
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(0));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "real:ok");
        assert_eq!(
            std::fs::read_to_string(capture).unwrap(),
            "rate-acquire gh\n"
        );
    }
}
