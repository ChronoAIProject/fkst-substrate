use serde_json::Value as JsonValue;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn command() -> Command {
    let mut command = Command::new(framework_bin());
    command.env_remove("FKST_SUPERVISOR_PID");
    command
}

fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn quoted(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!(r#""{value}""#))
        .collect::<Vec<_>>()
        .join(", ")
}

fn workspace(root: &Path, units: &[&str]) {
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = [{}]
"#,
            quoted(units)
        ),
    );
}

fn workspace_by_kind(root: &Path, packages: &[&str], libraries: &[&str]) {
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
packages = [{}]
libraries = [{}]
"#,
            quoted(packages),
            quoted(libraries)
        ),
    );
}

fn package(root: &Path, name: &str, libs: &[&str], events: &[&str]) {
    write(
        &root.join(format!("packages/{name}/fkst.toml")),
        &format!(
            r#"
kind = "package"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{}]

[event_deps]
packages = [{}]
"#,
            quoted(libs),
            quoted(events)
        ),
    );
}

fn library(root: &Path, name: &str, libs: &[&str], allow: Option<&[&str]>) {
    library_with_publishable(root, name, libs, allow, false);
}

fn library_with_publishable(
    root: &Path,
    name: &str,
    libs: &[&str],
    allow: Option<&[&str]>,
    publishable: bool,
) {
    let visibility = allow
        .map(|allow| {
            format!(
                r#"
[visibility]
allow = [{}]
"#,
                quoted(allow)
            )
        })
        .unwrap_or_default();
    let publishable = if publishable {
        "\npublishable = true"
    } else {
        ""
    };
    write(
        &root.join(format!("libraries/{name}/fkst.toml")),
        &format!(
            r#"
kind = "library"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{}]

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"{publishable}
{visibility}
"#,
            quoted(libs)
        ),
    );
}

fn deps(root: &Path) -> Command {
    let mut cmd = command();
    cmd.arg("deps").arg("--project-root").arg(root);
    cmd
}

fn host_lock(root: &Path) -> Command {
    let mut cmd = command();
    cmd.arg("host").arg("lock").arg("--project-root").arg(root);
    cmd
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn git_checked(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert_exit(&output, 0);
}

fn init_external_library_repo(root: &Path, visibility: Option<&[&str]>) -> String {
    workspace(root, &["libraries/contract"]);
    library_with_publishable(root, "contract", &[], visibility, true);
    write(
        &root.join("libraries/contract/public/api.lua"),
        r#"return { value = "external-contract" }"#,
    );
    write(
        &root.join("libraries/contract/private/secret.lua"),
        r#"return { value = "private-contract" }"#,
    );
    let init = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("init")
        .output()
        .unwrap();
    assert_exit(&init, 0);
    git(root, &["config", "user.email", "fkst-test@example.invalid"]);
    git(root, &["config", "user.name", "fkst test"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "Add external contract library"]);
    git(root, &["rev-parse", "HEAD"])
}

fn consumer_workspace_with_external_source(
    root: &Path,
    source_root: &Path,
    rev: &str,
    allowlist: &[&str],
    package_libs: &[&str],
) {
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = ["packages/app"]

[[external_sources]]
id = "fkst-platform"
git = "{}"
rev = "{rev}"
libraries = [{}]
"#,
            source_root.display(),
            quoted(allowlist)
        ),
    );
    package(root, "app", package_libs, &[]);
    write(
        &root.join("packages/app/departments/probe/main.lua"),
        r#"
local contract = require("contract.api")
return {
  spec = { consumes = { "tick" }, produces = {} },
  pipeline = function(event)
    assert(contract.value == "external-contract", contract.value)
  end,
}
"#,
    );
}

fn consumer_workspace_with_external_packages(
    root: &Path,
    source_root: &Path,
    rev: &str,
    packages: &[&str],
    libraries: &[&str],
) {
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = []

[[external_sources]]
id = "fkst-platform"
git = "{}"
rev = "{rev}"
packages = [{}]
libraries = [{}]
"#,
            source_root.display(),
            quoted(packages),
            quoted(libraries)
        ),
    );
}

fn init_platform_package_repo(root: &Path) -> String {
    workspace(root, &["packages/platform-pkg", "libraries/contract"]);
    package(root, "platform-pkg", &["contract"], &[]);
    write(
        &root.join("packages/platform-pkg/departments/probe/main.lua"),
        r#"
local contract = require("contract.api")
return {
  spec = { consumes = { "tick" }, produces = {} },
  pipeline = function(event)
    assert(contract.value == "external-contract", contract.value)
  end,
}
"#,
    );
    library_with_publishable(root, "contract", &[], None, true);
    write(
        &root.join("libraries/contract/public/api.lua"),
        r#"return { value = "external-contract" }"#,
    );
    let init = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("init")
        .output()
        .unwrap();
    assert_exit(&init, 0);
    git(root, &["config", "user.email", "fkst-test@example.invalid"]);
    git(root, &["config", "user.name", "fkst test"]);
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "Add platform package"]);
    git(root, &["rev-parse", "HEAD"])
}

fn consumer_workspace_with_external_tag(
    root: &Path,
    source_root: &Path,
    tag: &str,
    allowlist: &[&str],
    package_libs: &[&str],
) {
    write(
        &root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = ["packages/app"]

[[external_sources]]
id = "fkst-platform"
git = "{}"
tag = "{tag}"
libraries = [{}]
"#,
            source_root.display(),
            quoted(allowlist)
        ),
    );
    package(root, "app", package_libs, &[]);
    write(
        &root.join("packages/app/main.lua"),
        r#"return require("contract.api")"#,
    );
}

fn run_department(root: &Path, cache: &Path) -> Command {
    let mut cmd = command();
    cmd.arg("run")
        .arg(root.join("packages/app/departments/probe/main.lua"))
        .arg("--project-root")
        .arg(root)
        .arg("--package-root")
        .arg(root.join("packages/app"))
        .arg("--event")
        .arg(r#"{"queue":"tick","payload":{}}"#)
        .env("FKST_CACHE_ROOT", cache)
        .env("FKST_RUNTIME_ROOT", root.join(".fkst/runtime"));
    cmd
}

#[test]
fn host_lock_writes_lockfile_for_declared_external_sources() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let rev = init_external_library_repo(&source, None);
    consumer_workspace_with_external_source(&consumer, &source, &rev, &["contract"], &["contract"]);

    let lock_output = host_lock(&consumer)
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&lock_output, 0);
    let out = stdout(&lock_output);
    assert!(out.contains("fkst host lock: wrote"), "{out}");
    assert!(out.contains("fkst.lock"), "{out}");
    let lock = fs::read_to_string(consumer.join("fkst.lock")).unwrap();
    assert!(lock.contains("[[external_source]]"), "{lock}");
    assert!(lock.contains(r#"id = "fkst-platform""#), "{lock}");
    assert!(lock.contains(&format!(r#"rev = "{rev}""#)), "{lock}");

    let locked_output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&locked_output, 0);
}

#[test]
fn host_lock_with_package_root_pins_local_source_head() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let remote = temp.path().join("remote");
    let local = temp.path().join("local-platform");
    let consumer = temp.path().join("consumer");
    let remote_rev = init_platform_package_repo(&remote);
    let clone = Command::new("git")
        .arg("clone")
        .arg(&remote)
        .arg(&local)
        .output()
        .unwrap();
    assert_exit(&clone, 0);
    git(
        &local,
        &["config", "user.email", "fkst-test@example.invalid"],
    );
    git(&local, &["config", "user.name", "fkst test"]);
    write(
        &local.join("libraries/contract/public/api.lua"),
        r#"return { value = "local-contract" }"#,
    );
    git(&local, &["add", "."]);
    git(&local, &["commit", "-m", "Advance local platform"]);
    let local_rev = git(&local, &["rev-parse", "HEAD"]);
    assert_ne!(remote_rev, local_rev);
    consumer_workspace_with_external_packages(
        &consumer,
        &remote,
        &remote_rev,
        &["platform-pkg"],
        &["contract"],
    );

    let lock_output = host_lock(&consumer)
        .arg("--package-root")
        .arg(local.join("packages/platform-pkg"))
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&lock_output, 0);
    let lock = fs::read_to_string(consumer.join("fkst.lock")).unwrap();
    assert!(lock.contains(&format!(r#"rev = "{remote_rev}""#)), "{lock}");
    assert!(lock.contains(&format!(r#"rev = "{local_rev}""#)), "{lock}");
}

#[test]
fn locked_package_root_head_mismatch_uses_local_override() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let remote = temp.path().join("remote");
    let local = temp.path().join("local-platform");
    let consumer = temp.path().join("consumer");
    let remote_rev = init_platform_package_repo(&remote);
    let clone = Command::new("git")
        .arg("clone")
        .arg(&remote)
        .arg(&local)
        .output()
        .unwrap();
    assert_exit(&clone, 0);
    git(
        &local,
        &["config", "user.email", "fkst-test@example.invalid"],
    );
    git(&local, &["config", "user.name", "fkst test"]);
    write(
        &consumer.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = ["packages/app"]

[[external_sources]]
id = "fkst-platform"
git = "{}"
rev = "{remote_rev}"
packages = ["platform-pkg"]
libraries = ["contract"]
"#,
            remote.display()
        ),
    );
    package(&consumer, "app", &["contract"], &[]);
    write(
        &consumer.join("packages/app/departments/probe/main.lua"),
        r#"
local contract = require("contract.api")
return {
  spec = { consumes = { "tick" }, produces = {} },
  pipeline = function(event)
    assert(contract.value == "advanced-local-contract", contract.value)
  end,
}
"#,
    );

    let lock_output = host_lock(&consumer)
        .arg("--package-root")
        .arg(local.join("packages/platform-pkg"))
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&lock_output, 0);

    write(
        &local.join("libraries/contract/public/api.lua"),
        r#"return { value = "advanced-local-contract" }"#,
    );
    git(&local, &["add", "."]);
    git(
        &local,
        &["commit", "-m", "Advance local platform after lock"],
    );
    let advanced_rev = git(&local, &["rev-parse", "HEAD"]);

    let output = deps(&consumer)
        .arg("--package-root")
        .arg(local.join("packages/platform-pkg"))
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    let err = stderr(&output);
    assert!(
        !err.contains("resolved.rev does not match explicit --package-root source HEAD"),
        "{err}"
    );
    assert!(!err.contains(&advanced_rev), "{err}");

    let run_output = command()
        .arg("run")
        .arg(consumer.join("packages/app/departments/probe/main.lua"))
        .arg("--project-root")
        .arg(&consumer)
        .arg("--package-root")
        .arg(consumer.join("packages/app"))
        .arg("--package-root")
        .arg(local.join("packages/platform-pkg"))
        .arg("--owner-namespace")
        .arg("app")
        .arg("--event")
        .arg(r#"{"queue":"tick","payload":{}}"#)
        .env("FKST_CACHE_ROOT", &cache)
        .env("FKST_RUNTIME_ROOT", consumer.join(".fkst/runtime"))
        .output()
        .unwrap();

    assert_exit(&run_output, 0);
    assert!(
        !stderr(&run_output).contains("startup error"),
        "stderr: {}",
        stderr(&run_output)
    );
}

#[test]
fn cross_repo_deps_lock_writes_hashes_and_locked_catalog_resolves_external_library() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let rev = init_external_library_repo(&source, None);
    consumer_workspace_with_external_source(&consumer, &source, &rev, &["contract"], &["contract"]);

    let lock_output = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&lock_output, 0);
    let lock = fs::read_to_string(consumer.join("fkst.lock")).unwrap();
    assert!(lock.contains("[[external_source]]"), "{lock}");
    assert!(lock.contains(r#"id = "fkst-platform""#), "{lock}");
    assert!(lock.contains(&format!(r#"rev = "{rev}""#)), "{lock}");
    assert!(lock.contains("tree_sha256 = \"sha256-"), "{lock}");
    assert!(lock.contains("exports_sha256 = \"sha256-"), "{lock}");

    let locked_output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&locked_output, 0);
    let out = stdout(&locked_output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    assert!(out.contains("app -> contract"), "{out}");

    let run_output = run_department(&consumer, &cache).output().unwrap();

    assert_exit(&run_output, 0);
    assert!(
        !stderr(&run_output).contains("startup error"),
        "stderr: {}",
        stderr(&run_output)
    );
}

#[test]
fn cross_repo_cached_mirror_fetches_new_non_default_branch_commit() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let initial_rev = init_external_library_repo(&source, None);
    let default_branch = git(&source, &["branch", "--show-current"]);
    consumer_workspace_with_external_source(
        &consumer,
        &source,
        &initial_rev,
        &["contract"],
        &["contract"],
    );

    let initial_lock = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&initial_lock, 0);

    git_checked(&source, &["checkout", "-b", "feature/revision"]);
    write(
        &source.join("libraries/contract/public/api.lua"),
        r#"return { value = "feature-contract" }"#,
    );
    git_checked(&source, &["add", "."]);
    git_checked(&source, &["commit", "-m", "Advance feature branch"]);
    let feature_rev = git(&source, &["rev-parse", "HEAD"]);
    git_checked(&source, &["checkout", &default_branch]);
    consumer_workspace_with_external_source(
        &consumer,
        &source,
        &feature_rev,
        &["contract"],
        &["contract"],
    );

    let feature_lock = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&feature_lock, 0);
    let lock = fs::read_to_string(consumer.join("fkst.lock")).unwrap();
    assert!(lock.contains(&format!(r#"rev = "{feature_rev}""#)), "{lock}");
}

#[test]
fn cross_repo_unlocked_external_source_fails_closed_until_lock_is_written() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let rev = init_external_library_repo(&source, None);
    consumer_workspace_with_external_source(&consumer, &source, &rev, &["contract"], &["contract"]);

    let output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(
        err.contains("external source `fkst-platform` is missing from fkst.lock"),
        "{err}"
    );
    assert!(
        err.contains("fkst-framework host lock --project-root <root>"),
        "{err}"
    );
}

#[test]
fn cross_repo_non_publishable_library_is_not_externally_available_but_remains_intra_repo_visible() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let external_consumer = temp.path().join("external-consumer");

    workspace(&source, &["packages/app", "libraries/contract"]);
    package(&source, "app", &["contract"], &[]);
    write(
        &source.join("packages/app/main.lua"),
        r#"return require("contract.api")"#,
    );
    library(&source, "contract", &[], None);
    write(
        &source.join("libraries/contract/public/api.lua"),
        r#"return { value = "intra-repo-contract" }"#,
    );

    let intra_output = deps(&source).output().unwrap();
    assert_exit(&intra_output, 0);
    let intra_out = stdout(&intra_output);
    assert!(intra_out.contains("fkst deps: PASS"), "{intra_out}");
    assert!(intra_out.contains("app -> contract"), "{intra_out}");

    let init = Command::new("git")
        .arg("-C")
        .arg(&source)
        .arg("init")
        .output()
        .unwrap();
    assert_exit(&init, 0);
    git(
        &source,
        &["config", "user.email", "fkst-test@example.invalid"],
    );
    git(&source, &["config", "user.name", "fkst test"]);
    git(&source, &["add", "."]);
    git(
        &source,
        &["commit", "-m", "Add non-publishable contract library"],
    );
    let rev = git(&source, &["rev-parse", "HEAD"]);

    consumer_workspace_with_external_source(
        &external_consumer,
        &source,
        &rev,
        &["contract"],
        &["contract"],
    );

    let output = deps(&external_consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(
        err.contains("external source `fkst-platform` does not allow library `contract`"),
        "{err}"
    );
}

#[test]
fn cross_repo_non_allowlisted_or_non_visible_library_is_denied() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let non_allowlisted = temp.path().join("non-allowlisted");
    let non_visible = temp.path().join("non-visible");
    workspace(&source, &["libraries/contract", "libraries/other"]);
    library_with_publishable(&source, "contract", &[], Some(&["other-app"]), true);
    write(
        &source.join("libraries/contract/public/api.lua"),
        r#"return { value = "external-contract" }"#,
    );
    library_with_publishable(&source, "other", &[], None, true);
    write(
        &source.join("libraries/other/public/api.lua"),
        "return {}\n",
    );
    let init = Command::new("git")
        .arg("-C")
        .arg(&source)
        .arg("init")
        .output()
        .unwrap();
    assert_exit(&init, 0);
    git(
        &source,
        &["config", "user.email", "fkst-test@example.invalid"],
    );
    git(&source, &["config", "user.name", "fkst test"]);
    git(&source, &["add", "."]);
    git(
        &source,
        &["commit", "-m", "Add restricted external libraries"],
    );
    let rev = git(&source, &["rev-parse", "HEAD"]);

    consumer_workspace_with_external_source(
        &non_allowlisted,
        &source,
        &rev,
        &["other"],
        &["contract"],
    );
    let output = deps(&non_allowlisted)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(
        err.contains("external source `fkst-platform` does not allow library `contract`"),
        "{err}"
    );

    consumer_workspace_with_external_source(
        &non_visible,
        &source,
        &rev,
        &["contract"],
        &["contract"],
    );
    let output = deps(&non_visible)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(
        err.contains("unit `app` is not allowed to declare library `contract`"),
        "{err}"
    );
}

#[test]
fn cross_repo_locked_tree_hash_mismatch_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let rev = init_external_library_repo(&source, None);
    consumer_workspace_with_external_source(&consumer, &source, &rev, &["contract"], &["contract"]);
    let output = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let lock_path = consumer.join("fkst.lock");
    let mut lock = fs::read_to_string(&lock_path).unwrap();
    let marker = "tree_sha256 = \"sha256-";
    let idx = lock.find(marker).unwrap() + marker.len();
    let replacement = if lock.as_bytes()[idx] == b'0' {
        "1"
    } else {
        "0"
    };
    lock.replace_range(idx..idx + 1, replacement);
    fs::write(&lock_path, lock).unwrap();

    let output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(err.contains("tree hash mismatch"), "{err}");
}

#[test]
fn cross_repo_stale_lock_missing_allowlisted_library_fails_closed() {
    // A stale/tampered lock that omits a manifest-allowlisted external library
    // must fail closed under --locked. Otherwise the external provider is never
    // cataloged, the duplicate-name fail-closed check never fires, and an
    // internal library of the same name would be silently selected (violating
    // "no workspace-internal-wins precedence"). Regression guard for the
    // lock-allowlist-completeness check in validate_lock_matches_manifest.
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let rev = init_external_library_repo(&source, None);
    // Lock with an allowlist containing only `contract`.
    consumer_workspace_with_external_source(&consumer, &source, &rev, &["contract"], &["contract"]);
    let output = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    // Widen the manifest allowlist to include a library absent from the lock,
    // leaving the lock from the previous step unchanged.
    consumer_workspace_with_external_source(
        &consumer,
        &source,
        &rev,
        &["contract", "ghost"],
        &["contract"],
    );
    let output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 2);
    let err = stderr(&output);
    assert!(
        err.contains(
            "library `ghost` is allowed by the workspace manifest but missing from fkst.lock"
        ),
        "{err}"
    );
    assert!(
        err.contains("fkst-framework host lock --project-root <root>"),
        "{err}"
    );
}

#[test]
fn cross_repo_source_with_benign_symlink_locks_and_resolves() {
    // A real source repo can contain benign symlinks (e.g. a root AGENTS.md ->
    // CLAUDE.md). The tree hash must record a symlink by its target — neither
    // reject it nor dereference it — so the source locks and resolves. The
    // module index stays no-follow, so the symlink is inert at load time.
    // Regression for the dogfood-caught reject-symlink bug.
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let _ = init_external_library_repo(&source, None);
    write(&source.join("CLAUDE.md"), "doc body\n");
    std::os::unix::fs::symlink("CLAUDE.md", source.join("AGENTS.md")).unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "Add a benign root symlink"]);
    let rev = git(&source, &["rev-parse", "HEAD"]);
    consumer_workspace_with_external_source(&consumer, &source, &rev, &["contract"], &["contract"]);

    let lock_output = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&lock_output, 0);

    let locked_output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&locked_output, 0);
    let out = stdout(&locked_output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    assert!(out.contains("app -> contract"), "{out}");
}

#[test]
fn cross_repo_moved_tag_does_not_change_locked_build() {
    let temp = tempfile::tempdir().unwrap();
    let cache = temp.path().join("cache");
    let source = temp.path().join("source");
    let consumer = temp.path().join("consumer");
    let rev1 = init_external_library_repo(&source, None);
    git_checked(&source, &["tag", "-f", "contract-release", &rev1]);
    consumer_workspace_with_external_tag(
        &consumer,
        &source,
        "contract-release",
        &["contract"],
        &["contract"],
    );
    let output = deps(&consumer)
        .arg("lock")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let locked_before = fs::read_to_string(consumer.join("fkst.lock")).unwrap();
    assert!(
        locked_before.contains(&format!(r#"rev = "{rev1}""#)),
        "{locked_before}"
    );

    write(
        &source.join("libraries/contract/public/api.lua"),
        r#"return { value = "moved-tag-contract" }"#,
    );
    git_checked(&source, &["add", "."]);
    git_checked(&source, &["commit", "-m", "Move contract tag target"]);
    let rev2 = git(&source, &["rev-parse", "HEAD"]);
    assert_ne!(rev1, rev2);
    git_checked(&source, &["tag", "-f", "contract-release", &rev2]);

    let output = deps(&consumer)
        .arg("--locked")
        .env("FKST_CACHE_ROOT", &cache)
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let locked_after = fs::read_to_string(consumer.join("fkst.lock")).unwrap();
    assert_eq!(locked_before, locked_after);
}

#[test]
fn deps_passes_valid_workspace_and_reports_warnings() {
    let temp = tempfile::tempdir().unwrap();
    workspace(
        temp.path(),
        &[
            "packages/valid",
            "packages/unused",
            "libraries/std",
            "libraries/extra",
        ],
    );
    package(temp.path(), "valid", &["std"], &["unused"]);
    write(
        &temp.path().join("packages/valid/main.lua"),
        r#"
local json = require("std.fkst.json")
return json
"#,
    );
    package(temp.path(), "unused", &["extra"], &[]);
    write(&temp.path().join("packages/unused/main.lua"), "return {}\n");
    library(temp.path(), "std", &[], None);
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );
    library(temp.path(), "extra", &[], None);
    write(
        &temp.path().join("libraries/extra/public/tool.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    assert!(out.contains("valid -> std"), "{out}");
    assert!(out.contains("[unused-lib-dep]"), "{out}");
    assert!(out.contains("unused declares library `extra`"), "{out}");
}

#[test]
fn deps_counts_declared_bare_library_root_require() {
    let temp = tempfile::tempdir().unwrap();
    workspace(temp.path(), &["packages/app", "libraries/contract"]);
    package(temp.path(), "app", &["contract"], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"return require("contract")"#,
    );
    library(temp.path(), "contract", &[], None);
    write(
        &temp.path().join("libraries/contract/public/init.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    assert!(!out.contains("[unused-lib-dep]"), "{out}");
}

#[test]
fn deps_reports_undeclared_bare_library_root_require() {
    let temp = tempfile::tempdir().unwrap();
    workspace(temp.path(), &["packages/app", "libraries/contract"]);
    package(temp.path(), "app", &[], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"return require("contract")"#,
    );
    library(temp.path(), "contract", &[], None);
    write(
        &temp.path().join("libraries/contract/public/init.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(out.contains("[undeclared-require]"), "{out}");
    assert!(out.contains("app requires library `contract`"), "{out}");
}

#[test]
fn deps_reports_missing_bare_library_root_export() {
    let temp = tempfile::tempdir().unwrap();
    workspace(temp.path(), &["packages/app", "libraries/contract"]);
    package(temp.path(), "app", &["contract"], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"return require("contract")"#,
    );
    library(temp.path(), "contract", &[], None);
    write(
        &temp.path().join("libraries/contract/public/api.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(out.contains("[missing-export]"), "{out}");
    assert!(
        out.contains("app references missing public export `contract`"),
        "{out}"
    );
}

#[test]
fn deps_excludes_bare_self_library_require() {
    let temp = tempfile::tempdir().unwrap();
    workspace(temp.path(), &["libraries/contract"]);
    library(temp.path(), "contract", &[], None);
    write(
        &temp.path().join("libraries/contract/public/init.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/contract/private/helper.lua"),
        r#"return require("contract")"#,
    );

    let output = deps(temp.path()).arg("--json").output().unwrap();

    assert_exit(&output, 0);
    let value: JsonValue = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["units"][0]["actual_lib_requires"],
        serde_json::json!([])
    );
    assert_eq!(value["failures"], serde_json::json!([]));
}

#[test]
fn deps_fails_for_undeclared_require_visibility_violation_and_cycle() {
    let temp = tempfile::tempdir().unwrap();
    workspace(
        temp.path(),
        &[
            "packages/valid",
            "packages/bad",
            "packages/two-libs",
            "libraries/std",
            "libraries/restricted",
            "libraries/alpha",
            "libraries/beta",
            "libraries/cycle-a",
            "libraries/cycle-b",
        ],
    );
    package(temp.path(), "valid", &["std"], &[]);
    write(
        &temp.path().join("packages/valid/main.lua"),
        r#"return require("std.fkst.json")"#,
    );
    package(temp.path(), "bad", &["restricted", "ghost"], &[]);
    write(
        &temp.path().join("packages/bad/main.lua"),
        r#"
local json = require("std.fkst.json")
local missing = require("restricted.missing")
return { json = json, missing = missing }
"#,
    );
    package(temp.path(), "two-libs", &["alpha", "beta"], &[]);
    write(
        &temp.path().join("packages/two-libs/main.lua"),
        r#"
local alpha = require("alpha.shared")
local beta = require("beta.shared")
return { alpha = alpha, beta = beta }
"#,
    );
    library(temp.path(), "std", &[], None);
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );
    library(temp.path(), "restricted", &[], Some(&["valid"]));
    write(
        &temp.path().join("libraries/restricted/public/tool.lua"),
        "return {}\n",
    );
    library(temp.path(), "alpha", &[], None);
    write(
        &temp.path().join("libraries/alpha/public/shared.lua"),
        "return {}\n",
    );
    library(temp.path(), "beta", &[], None);
    write(
        &temp.path().join("libraries/beta/public/shared.lua"),
        "return {}\n",
    );
    library(temp.path(), "cycle-a", &["cycle-b"], None);
    write(
        &temp.path().join("libraries/cycle-a/public/a.lua"),
        "return {}\n",
    );
    library(temp.path(), "cycle-b", &["cycle-a"], None);
    write(
        &temp.path().join("libraries/cycle-b/public/b.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: FAIL"), "{out}");
    assert!(out.contains("[cycle]"), "{out}");
    assert!(out.contains("[missing-lib]"), "{out}");
    assert!(out.contains("[visibility]"), "{out}");
    assert!(
        out.contains("bad is not allowed to declare library `restricted`"),
        "{out}"
    );
    assert!(out.contains("[undeclared-require]"), "{out}");
    assert!(out.contains("bad requires library `std`"), "{out}");
    assert!(out.contains("[missing-export]"), "{out}");
    assert!(stderr(&output).is_empty(), "stderr: {}", stderr(&output));
}

#[test]
fn deps_json_output_has_stable_shape() {
    let temp = tempfile::tempdir().unwrap();
    workspace(temp.path(), &["packages/app", "libraries/std"]);
    package(temp.path(), "app", &["std"], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"return require("std.fkst.json")"#,
    );
    library(temp.path(), "std", &[], None);
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );

    let output = deps(temp.path()).arg("--json").output().unwrap();

    assert_exit(&output, 0);
    let value: JsonValue = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], JsonValue::Bool(true));
    assert!(value["workspace_root"]
        .as_str()
        .unwrap()
        .contains(temp.path().file_name().unwrap().to_str().unwrap()));
    assert_eq!(value["units"].as_array().unwrap().len(), 2);
    assert_eq!(value["lib_edges"].as_array().unwrap().len(), 1);
    assert_eq!(value["event_edges"].as_array().unwrap().len(), 0);
    assert_eq!(value["failures"].as_array().unwrap().len(), 0);
    assert_eq!(value["warnings"].as_array().unwrap().len(), 0);
}

#[test]
fn deps_accepts_workspace_package_and_library_lists_for_flat_library_exports() {
    let temp = tempfile::tempdir().unwrap();
    workspace_by_kind(temp.path(), &["packages/app"], &["std"]);
    package(temp.path(), "app", &["std"], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"
local a = require("std.a")
local b = require("std.sub.b")
return { a = a, b = b }
"#,
    );
    write(
        &temp.path().join("std/fkst.toml"),
        r#"
kind = "library"
name = "std"

[code]
root = "."

[library]
name = "std"
stable_id = "std"
version = "0.1.0"
"#,
    );
    write(&temp.path().join("std/a.lua"), "return {}\n");
    write(
        &temp.path().join("std/sub/b.lua"),
        r#"return require("std.a")"#,
    );

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: PASS"), "{out}");
    assert!(out.contains("public_exports: std.a, std.sub.b"), "{out}");
}

#[test]
fn deps_reports_missing_prefixed_flat_library_export() {
    let temp = tempfile::tempdir().unwrap();
    workspace_by_kind(temp.path(), &["packages/app"], &["std"]);
    package(temp.path(), "app", &["std"], &[]);
    write(
        &temp.path().join("packages/app/main.lua"),
        r#"return require("std.x")"#,
    );
    write(
        &temp.path().join("std/fkst.toml"),
        r#"
kind = "library"
name = "std"

[code]
root = "."

[library]
name = "std"
stable_id = "std"
version = "0.1.0"
"#,
    );
    write(&temp.path().join("std/a.lua"), "return {}\n");

    let output = deps(temp.path()).output().unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(out.contains("[missing-export]"), "{out}");
    assert!(
        out.contains("app references missing public export `std.x`"),
        "{out}"
    );
}

#[test]
fn deps_validates_explicit_external_package_catalog() {
    let temp = tempfile::Builder::new()
        .prefix("deps-external")
        .tempdir()
        .unwrap();
    let host = temp.path().join("host");
    let external = temp.path().join("platform");
    let external_package = external.join("packages/platform-pkg");
    fs::create_dir_all(&host).unwrap();
    fs::create_dir_all(&external_package).unwrap();
    workspace(&host, &[]);
    workspace(
        &external,
        &["packages/platform-pkg", "libraries/std", "libraries/extra"],
    );
    package(&external, "platform-pkg", &[], &["declared"]);
    write(
        &external_package.join("main.lua"),
        r#"
local json = require("std.fkst.json")
return json
"#,
    );
    library(&external, "std", &[], None);
    write(
        &external.join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );
    library(&external, "extra", &[], None);
    write(
        &external.join("libraries/extra/public/tool.lua"),
        "return {}\n",
    );

    let output = deps(&host)
        .arg("--package-root")
        .arg(&external_package)
        .output()
        .unwrap();

    assert_exit(&output, 1);
    let out = stdout(&output);
    assert!(out.contains("fkst deps: FAIL"), "{out}");
    assert!(out.contains("platform-pkg requires library `std`"), "{out}");
}

#[test]
fn deps_help_prints_usage() {
    let output = command().arg("deps").arg("--help").output().unwrap();

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(
        out.contains("fkst-framework deps [lock|fetch] --project-root <root>"),
        "{out}"
    );
    assert!(out.contains("--json"), "{out}");
    assert!(out.contains("--locked"), "{out}");
}
