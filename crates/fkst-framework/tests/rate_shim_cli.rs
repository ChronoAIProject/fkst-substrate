#[path = "../src/config_registry.rs"]
mod config_registry;
#[path = "../src/rate_pool.rs"]
mod rate_pool;
#[path = "../src/rate_shim.rs"]
mod rate_shim;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rate_pool::{RatePoolConfig, RatePoolRegistry};

struct EnvGuard {
    values: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

struct CwdGuard {
    original: PathBuf,
}

impl CwdGuard {
    fn enter(path: &Path) -> Self {
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

impl EnvGuard {
    fn remove(keys: &[&'static str]) -> Self {
        let values = keys
            .iter()
            .map(|key| {
                let value = std::env::var_os(key);
                std::env::remove_var(key);
                (*key, value)
            })
            .collect();
        Self { values }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.values.drain(..).rev() {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}

fn framework_bin() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps_dir = exe.parent().unwrap();
    deps_dir.parent().unwrap().join("fkst-framework")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

#[cfg(unix)]
fn executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, body).unwrap();
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn ledger_tokens(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("tokens="))
        .unwrap()
        .parse()
        .unwrap()
}

fn seed_ledger(root: &Path, name: &str, tokens: u64) {
    std::fs::create_dir_all(root).unwrap();
    let updated_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::fs::write(
        root.join(format!("{name}.bucket")),
        format!("updated_nanos={updated_nanos}\ntokens={tokens}\nremainder_nanos=0\n"),
    )
    .unwrap();
}

#[test]
fn rate_acquire_unconfigured_pool_is_passthrough() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(framework_bin())
        .arg("rate-acquire")
        .arg("gh")
        .env("FKST_RATE_POOL_ROOT", tmp.path())
        .env_remove("FKST_RATE_POOL_GH")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(!tmp.path().join("gh.bucket").exists());
}

#[test]
fn rate_acquire_rejects_invalid_config() {
    let tmp = tempfile::tempdir().unwrap();
    let output = Command::new(framework_bin())
        .arg("rate-acquire")
        .arg("gh")
        .env("FKST_RATE_POOL_ROOT", tmp.path())
        .env("FKST_RATE_POOL_GH", "bad")
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(
        stderr(&output).contains("FKST_RATE_POOL_GH must use '<burst>,<refill_per_minute>'"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn rate_acquire_consumes_configured_pool_token() {
    let tmp = tempfile::tempdir().unwrap();
    seed_ledger(tmp.path(), "gh", 2);
    let output = Command::new(framework_bin())
        .arg("rate-acquire")
        .arg("gh")
        .env("FKST_RATE_POOL_ROOT", tmp.path())
        .env("FKST_RATE_POOL_GH", "2,1")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert_eq!(ledger_tokens(&tmp.path().join("gh.bucket")), 1);
}

#[cfg(unix)]
#[test]
fn shim_resolver_skips_shim_dir_to_avoid_recursion() {
    let tmp = tempfile::tempdir().unwrap();
    let shim_dir = tmp.path().join("shims");
    let real_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&shim_dir).unwrap();
    std::fs::create_dir_all(&real_dir).unwrap();
    executable(&shim_dir.join("gh"), "#!/bin/sh\nexit 70\n");
    executable(&real_dir.join("gh"), "#!/bin/sh\nprintf real\n");
    let path = std::env::join_paths([shim_dir.as_path(), real_dir.as_path()]).unwrap();

    let resolved = rate_shim::resolve_program_on_path("gh", Some(&path), &shim_dir).unwrap();

    assert_eq!(resolved, real_dir.join("gh"));
}

#[cfg(unix)]
#[test]
fn generated_shim_consumes_same_bucket_as_cli_acquire() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("rate-pools");
    let real_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&real_dir).unwrap();
    executable(&real_dir.join("gh"), "#!/bin/sh\nprintf shim-real\n");
    let registry = RatePoolRegistry::for_test(
        root.clone(),
        BTreeMap::from([(
            "gh".to_string(),
            RatePoolConfig {
                burst: 2,
                refill_per_minute: 1,
            },
        )]),
    );
    seed_ledger(&root, "gh", 2);

    let generator_path = std::env::join_paths([real_dir.as_path()]).unwrap();
    let shim_dir =
        rate_shim::ensure_rate_shims_with_path(&registry, &framework_bin(), &generator_path)
            .unwrap();

    let direct = Command::new(framework_bin())
        .arg("rate-acquire")
        .arg("gh")
        .env("FKST_RATE_POOL_ROOT", &root)
        .env("FKST_RATE_POOL_GH", "2,1")
        .output()
        .unwrap();
    assert_exit(&direct, 0);
    assert_eq!(ledger_tokens(&root.join("gh.bucket")), 1);

    let path = std::env::join_paths([shim_dir.as_path(), real_dir.as_path()]).unwrap();
    let shim = Command::new(shim_dir.join("gh"))
        .env("PATH", path)
        .env("FKST_RATE_POOL_ROOT", &root)
        .env("FKST_RATE_POOL_GH", "2,1")
        .output()
        .unwrap();

    assert_exit(&shim, 0);
    assert_eq!(stdout(&shim), "shim-real");
    assert_eq!(ledger_tokens(&root.join("gh.bucket")), 0);
}

#[cfg(unix)]
#[test]
fn generated_shim_bakes_resolved_config_from_fkst_env_style_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let host = tmp.path().join("host");
    let run_cwd = tmp.path().join("run-cwd");
    let real_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&host).unwrap();
    std::fs::create_dir_all(&run_cwd).unwrap();
    std::fs::create_dir_all(&real_dir).unwrap();
    std::fs::write(
        host.join("fkst.env"),
        "FKST_RATE_POOL_ROOT=relative-rate-pools\nFKST_RATE_POOL_GH=2,1\n",
    )
    .unwrap();
    executable(&real_dir.join("gh"), "#!/bin/sh\nprintf shim-real\n");

    let _env_guard = EnvGuard::remove(&["FKST_RATE_POOL_ROOT", "FKST_RATE_POOL_GH"]);
    let _cwd_guard = CwdGuard::enter(&host);
    let config = config_registry::ConfigContext::from_host_root(&host).unwrap();
    let registry = RatePoolRegistry::from_config(&config).unwrap();
    drop(_cwd_guard);
    seed_ledger(registry.root(), "gh", 2);
    let generator_path = std::env::join_paths([real_dir.as_path()]).unwrap();
    let shim_dir =
        rate_shim::ensure_rate_shims_with_path(&registry, &framework_bin(), &generator_path)
            .unwrap();
    let path = std::env::join_paths([shim_dir.as_path(), real_dir.as_path()]).unwrap();

    let shim = Command::new(shim_dir.join("gh"))
        .current_dir(&run_cwd)
        .env("PATH", path)
        .env_remove("FKST_RATE_POOL_ROOT")
        .env_remove("FKST_RATE_POOL_GH")
        .output()
        .unwrap();

    assert_exit(&shim, 0);
    assert_eq!(stdout(&shim), "shim-real");
    assert_eq!(ledger_tokens(&registry.root().join("gh.bucket")), 1);
    assert!(!run_cwd.join("relative-rate-pools/gh.bucket").exists());
}

#[cfg(unix)]
#[test]
fn generated_shim_skips_symlinked_shim_path_before_real_program() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("rate-pools");
    let real_dir = tmp.path().join("bin");
    let count_file = tmp.path().join("count");
    std::fs::create_dir_all(&real_dir).unwrap();
    executable(
        &real_dir.join("gh"),
        &format!(
            "#!/bin/sh\ncount=$(cat {} 2>/dev/null || printf 0)\ncount=$((count + 1))\nprintf '%s' \"$count\" > {}\nprintf real-once\n",
            count_file.display(),
            count_file.display()
        ),
    );
    let registry = RatePoolRegistry::for_test(
        root.clone(),
        BTreeMap::from([(
            "gh".to_string(),
            RatePoolConfig {
                burst: 2,
                refill_per_minute: 1,
            },
        )]),
    );
    seed_ledger(&root, "gh", 2);
    let generator_path = std::env::join_paths([real_dir.as_path()]).unwrap();
    let shim_dir =
        rate_shim::ensure_rate_shims_with_path(&registry, &framework_bin(), &generator_path)
            .unwrap();
    let shim_link = tmp.path().join("shims-link");
    std::os::unix::fs::symlink(&shim_dir, &shim_link).unwrap();
    let path = std::env::join_paths([shim_link.as_path(), shim_dir.as_path(), real_dir.as_path()])
        .unwrap();

    let shim = Command::new(shim_dir.join("gh"))
        .env("PATH", path)
        .env_remove("FKST_RATE_POOL_ROOT")
        .env_remove("FKST_RATE_POOL_GH")
        .output()
        .unwrap();

    assert_exit(&shim, 0);
    assert_eq!(stdout(&shim), "real-once");
    assert_eq!(std::fs::read_to_string(count_file).unwrap(), "1");
    assert_eq!(ledger_tokens(&root.join("gh.bucket")), 1);
}
