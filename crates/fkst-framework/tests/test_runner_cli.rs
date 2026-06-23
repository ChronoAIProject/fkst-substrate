use base64::Engine;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

use support::manifest_fixture::{write_single_package_workspace, write_workspace_for_roots};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn framework_command() -> Command {
    let mut command = Command::new(framework_bin());
    command.env_remove("FKST_SUPERVISOR_PID");
    command
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path);
        } else {
            fs::copy(&src_path, &dst_path).unwrap();
        }
    }
}

fn copy_codex_package(host: &Path) {
    copy_dir(&repo_root().join("examples/codex-package"), host);
}

fn run_lua_tests(host: &Path, package: &Path) -> Output {
    if host == package {
        write_single_package_workspace(host);
    } else {
        write_workspace_for_roots(host, &[package]);
    }
    framework_command()
        .arg("test")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(package)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn run_lua_tests_with_packages(host: &Path, packages: &[&Path]) -> Output {
    write_workspace_for_roots(host, packages);
    let mut cmd = framework_command();
    cmd.arg("test").arg("--project-root").arg(host);
    for package in packages {
        cmd.arg("--package-root").arg(package);
    }
    cmd.current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn run_lua_tests_with_report(host: &Path, package: &Path, report: &Path) -> Output {
    if host == package {
        write_single_package_workspace(host);
    } else {
        write_workspace_for_roots(host, &[package]);
    }
    framework_command()
        .arg("test")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(package)
        .arg("--report-json")
        .arg(report)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn run_lua_tests_with_packages_and_report(
    host: &Path,
    packages: &[&Path],
    report: &Path,
) -> Output {
    write_workspace_for_roots(host, packages);
    let mut cmd = framework_command();
    cmd.arg("test").arg("--project-root").arg(host);
    for package in packages {
        cmd.arg("--package-root").arg(package);
    }
    cmd.arg("--report-json")
        .arg(report)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn run_lua_tests_with_coverage(host: &Path, package: &Path, coverage: &Path) -> Output {
    if host == package {
        write_single_package_workspace(host);
    } else {
        write_workspace_for_roots(host, &[package]);
    }
    framework_command()
        .arg("test")
        .arg("--project-root")
        .arg(host)
        .arg("--package-root")
        .arg(package)
        .arg("--coverage")
        .arg(coverage)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .output()
        .unwrap()
}

fn read_report(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn raised_entries(output: &Output) -> serde_json::Value {
    let out = stdout(output);
    let line = out
        .lines()
        .find_map(|line| line.strip_prefix("RAISED: "))
        .unwrap_or_else(|| panic!("missing RAISED line in stdout: {out}"));
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(line)
        .unwrap();
    serde_json::from_slice(&decoded).unwrap()
}

fn run_command(host: &Path, lua: &Path) -> Command {
    let mut cmd = framework_command();
    cmd.arg("run")
        .arg(lua)
        .arg("--project-root")
        .arg(host)
        .arg("--event")
        .arg(r#"{"payload":{"value":"ok"}}"#)
        .current_dir(host)
        .env("FKST_RUNTIME_ROOT", host.join(".fkst/runtime"))
        .env_remove("FKST_PACKAGE_ROOT")
        .env_remove("FKST_PACKAGE_ROOTS");
    cmd
}

#[test]
fn production_run_with_composed_package_roots_raises_declared_cross_package_queue() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    let owner = temp.path().join("github-devloop");
    let sibling = temp.path().join("consensus");
    fs::create_dir_all(owner.join("departments/probe")).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::create_dir_all(&host).unwrap();
    let probe = owner.join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}

function pipeline(event)
  raise("consensus.proposal", { source = event.payload.value })
end
return M
"#,
    )
    .unwrap();

    write_workspace_for_roots(&host, &[&owner, &sibling]);
    let output = run_command(&host, &probe)
        .arg("--package-root")
        .arg(&owner)
        .arg("--package-root")
        .arg(&sibling)
        .arg("--owner-namespace")
        .arg("github-devloop")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let raises = raised_entries(&output);
    assert_eq!(raises[0]["queue"], "consensus.proposal");
    assert_eq!(raises[0]["payload"]["source"], "ok");
}

#[test]
fn production_run_uses_same_resolved_produces_contract_as_test() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("pkg");
    fs::create_dir_all(package.join("departments/probe")).unwrap();
    let probe = package.join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "pkg.seen" } }
function pipeline(event)
  raise("pkg.seen", { value = event.payload.value })
end
return M
"#,
    )
    .unwrap();

    write_single_package_workspace(&package);
    let output = run_command(&package, &probe)
        .arg("--package-root")
        .arg(&package)
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let raises = raised_entries(&output);
    assert_eq!(raises[0]["queue"], "pkg.seen");
    assert_eq!(raises[0]["payload"]["value"], "ok");
}

#[test]
fn production_run_resolves_owner_locale_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    let owner = temp.path().join("github-devloop");
    fs::create_dir_all(owner.join("departments/probe")).unwrap();
    fs::create_dir_all(owner.join("locales")).unwrap();
    fs::create_dir_all(&host).unwrap();
    fs::write(
        owner.join("locales/en.lua"),
        r#"return { ["result.summary"] = "Hello {name}" }"#,
    )
    .unwrap();
    fs::write(
        owner.join("locales/zh-CN.lua"),
        r#"return { ["result.summary"] = "你好 {name}" }"#,
    )
    .unwrap();
    let probe = owner.join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "checked" } }
function pipeline(event)
  raise("checked", { message = t("result.summary", { name = event.payload.value }) })
end
return M
"#,
    )
    .unwrap();

    write_workspace_for_roots(&host, &[&owner]);
    let output = run_command(&host, &probe)
        .arg("--package-root")
        .arg(&owner)
        .env("FKST_OUTPUT_LANG", "zh-CN")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let raises = raised_entries(&output);
    assert_eq!(raises[0]["payload"]["message"], "你好 ok");
}

#[test]
fn production_run_composed_namespace_roots_do_not_authorize_sibling_require() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    let owner = temp.path().join("github-devloop");
    let sibling = temp.path().join("consensus");
    fs::create_dir_all(owner.join("departments/probe")).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::create_dir_all(&host).unwrap();
    fs::write(
        sibling.join("sibling_only.lua"),
        r#"return { value = "leaked" }"#,
    )
    .unwrap();
    let probe = owner.join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "checked" } }
function pipeline(event)
  local ok, err = pcall(require, "sibling_only")
  assert(not ok, "sibling module leaked into owner package.path")
  err = tostring(err)
  assert(string.find(err, "require.denied", 1, true), err)
  raise("checked", { isolated = true })
end
return M
"#,
    )
    .unwrap();

    write_workspace_for_roots(&host, &[&owner, &sibling]);
    let output = run_command(&host, &probe)
        .arg("--package-root")
        .arg(&owner)
        .arg("--package-root")
        .arg(&sibling)
        .arg("--owner-namespace")
        .arg("github-devloop")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let raises = raised_entries(&output);
    assert_eq!(raises[0]["queue"], "github-devloop.checked");
    assert_eq!(raises[0]["payload"]["isolated"], true);
}

#[test]
fn graph_json_returns_stable_composed_topology_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    let alpha = temp.path().join("alpha");
    let beta = temp.path().join("beta");
    fs::create_dir_all(host.join("departments/dashboard")).unwrap();
    fs::create_dir_all(host.join("raisers")).unwrap();
    fs::create_dir_all(alpha.join("departments/producer")).unwrap();
    fs::create_dir_all(alpha.join("raisers")).unwrap();
    fs::create_dir_all(beta.join("departments/consumer")).unwrap();
    fs::create_dir_all(&host).unwrap();
    fs::write(
        host.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=8\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=45s\nFKST_CODEX_PERMIT_SLOTS=3\nFKST_RETRY_DEFAULT_MAX_ATTEMPTS=5\nFKST_RETRY_DEFAULT_BASE=2s\nFKST_RETRY_DEFAULT_CAP=20s\n",
    )
    .unwrap();
    fs::write(
        alpha.join("departments/producer/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "beta.jobs" },
  ephemeral = { "tick" },
  fanout = { "tick" },
  stall_window = "9s",
  retry = { max_attempts = 2, base = "1s", cap = "4s" },
}
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    fs::write(
        beta.join("departments/consumer/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "jobs" },
  produces = { "host.render" },
  published_seam = { "jobs" },
  ephemeral = { "jobs" },
  retry = false,
}
function pipeline(_) end
return M
"#,
    )
    .unwrap();
    let dashboard = host.join("departments/dashboard/main.lua");
    fs::write(
        &dashboard,
        r#"
local M = {}
M.spec = {
  consumes = { "render" },
  produces = { "snapshot" },
  published_seam = { "render" },
  ephemeral = { "render" },
  stall_window = "7s",
  graph_json = true,
}
function pipeline(_)
  raise("snapshot", { graph = json.decode(graph_json()) })
end
return M
"#,
    )
    .unwrap();
    fs::write(
        alpha.join("raisers/tick.lua"),
        r#"return { type = "cron", interval = "10s", produces = "tick" }"#,
    )
    .unwrap();
    fs::write(
        host.join("raisers/render_file.lua"),
        r#"return { type = "file_watch", glob = "input/*.json", produces = "render" }"#,
    )
    .unwrap();

    write_workspace_for_roots(&host, &[&alpha, &beta]);
    let output = run_command(&host, &dashboard)
        .arg("--package-root")
        .arg(&alpha)
        .arg("--package-root")
        .arg(&beta)
        .arg("--owner-namespace")
        .arg("host")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let raises = raised_entries(&output);
    let graph = &raises[0]["payload"]["graph"];
    assert_eq!(
        graph,
        &serde_json::json!({
            "schema": "fkst.graph.v1",
            "nodes": [
                {
                    "kind": "raiser",
                    "id": "raiser:alpha.tick",
                    "name": "tick",
                    "package": "alpha",
                    "source": { "type": "cron", "interval": "10s" }
                },
                {
                    "kind": "raiser",
                    "id": "raiser:host.render_file",
                    "name": "render_file",
                    "package": "host",
                    "source": { "type": "file_watch", "glob": "input/*.json" }
                },
                {
                    "kind": "queue",
                    "id": "queue:alpha.tick",
                    "name": "tick",
                    "package": "alpha",
                    "fanout": true
                },
                {
                    "kind": "queue",
                    "id": "queue:beta.jobs",
                    "name": "jobs",
                    "package": "beta",
                    "fanout": false
                },
                {
                    "kind": "queue",
                    "id": "queue:host.render",
                    "name": "render",
                    "package": "host",
                    "fanout": false
                },
                {
                    "kind": "queue",
                    "id": "queue:host.snapshot",
                    "name": "snapshot",
                    "package": "host",
                    "fanout": false
                },
                {
                    "kind": "department",
                    "id": "department:alpha.producer",
                    "name": "producer",
                    "package": "alpha",
                    "consumes": ["alpha.tick"],
                    "produces": ["beta.jobs"],
                    "ephemeral": ["alpha.tick"],
                    "stall_window": "9s",
                    "retry": { "max_attempts": 2, "base": "1s", "cap": "4s" }
                },
                {
                    "kind": "department",
                    "id": "department:beta.consumer",
                    "name": "consumer",
                    "package": "beta",
                    "consumes": ["beta.jobs"],
                    "produces": ["host.render"],
                    "ephemeral": ["beta.jobs"],
                    "stall_window": "45s"
                },
                {
                    "kind": "department",
                    "id": "department:host.dashboard",
                    "name": "dashboard",
                    "package": "host",
                    "consumes": ["host.render"],
                    "produces": ["host.snapshot"],
                    "ephemeral": ["host.render"],
                    "stall_window": "7s",
                    "retry": { "max_attempts": 5, "base": "2s", "cap": "20s" }
                }
            ],
            "edges": [
                { "from": "department:alpha.producer", "to": "queue:beta.jobs", "relation": "produces" },
                { "from": "department:beta.consumer", "to": "queue:host.render", "relation": "produces" },
                { "from": "department:host.dashboard", "to": "queue:host.snapshot", "relation": "produces" },
                { "from": "queue:alpha.tick", "to": "department:alpha.producer", "relation": "consumes" },
                { "from": "queue:beta.jobs", "to": "department:beta.consumer", "relation": "consumes" },
                { "from": "queue:host.render", "to": "department:host.dashboard", "relation": "consumes" },
                { "from": "raiser:alpha.tick", "to": "queue:alpha.tick", "relation": "raises" },
                { "from": "raiser:host.render_file", "to": "queue:host.render", "relation": "raises" }
            ]
        })
    );
}

#[test]
fn graph_json_requires_department_spec_authorization() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    fs::create_dir_all(host.join("departments/dashboard")).unwrap();
    fs::write(
        host.join("fkst.env"),
        "FKST_QUEUE_CAPACITY=8\nFKST_DEPARTMENT_DEFAULT_STALL_WINDOW=30s\nFKST_CODEX_PERMIT_SLOTS=1\n",
    )
    .unwrap();
    fs::write(
        host.join("departments/dashboard/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "render" },
  produces = { "snapshot" },
  stall_window = "7s",
}
function pipeline(_)
  graph_json()
end
return M
"#,
    )
    .unwrap();

    write_single_package_workspace(&host);
    let output = run_command(&host, &host.join("departments/dashboard/main.lua"))
        .arg("--package-root")
        .arg(&host)
        .arg("--owner-namespace")
        .arg("host")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("graph_json requires M.spec.graph_json = true"),
        "stderr: {}",
        stderr(&output)
    );
}

fn namespace(root: &Path) -> String {
    root.canonicalize()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn test_runner_runs_codex_package_tests() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    copy_codex_package(host.path());

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS departments/codex_demo/codex_demo_test.lua::test_build"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_runs_minimal_package_sanity_tests() {
    let package = repo_root().join("examples/minimal-package");

    let output = run_lua_tests(&package, &package);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_sanity"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_raises"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_nil"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_json_decode"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_json_decode_invalid_input_raises"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_restricted_lua_load_returns_plain_data"),
        "stdout: {out}"
    );
    assert!(
        out.contains(
            "PASS tests/sanity_test.lua::test_restricted_lua_load_blocks_ambient_capabilities"
        ),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/sanity_test.lua::test_restricted_lua_load_uses_explicit_bindings"),
        "stdout: {out}"
    );
    assert!(
        out.contains(
            "PASS tests/sanity_test.lua::test_restricted_lua_load_rejects_bytecode_by_default"
        ),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/run_department_test.lua::test_run_department_captures_raises"),
        "stdout: {out}"
    );
    assert!(out.contains("10 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_exposes_only_the_explicit_hermetic_test_surface() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/fkst_test_surface_test.lua"),
        r#"
local t = fkst.test
local allowed = {
  command_calls = true,
  eq = true,
  is_nil = true,
  is_true = true,
  mock_command = true,
  raises = true,
  fire_raiser = true,
  run_department = true,
  with_command_cassette = true,
}

return {
  test_fkst_test_surface_is_explicit = function()
    for name, _ in pairs(t) do
      t.is_true(allowed[name], "unexpected fkst.test helper: " .. tostring(name))
    end
    for name, _ in pairs(allowed) do
      t.is_true(type(t[name]) == "function", "missing fkst.test helper: " .. name)
    end
    t.is_nil(t.use_cassette)
    t.is_nil(t.record_command)
    t.is_nil(t.replay_command)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS tests/fkst_test_surface_test.lua::test_fkst_test_surface_is_explicit"),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn fire_raiser_uses_real_cron_tick_and_surfaces_consumer_result() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/reject_tick")).unwrap();
    fs::create_dir_all(host.path().join("departments/accept_tick")).unwrap();
    fs::create_dir_all(host.path().join("raisers")).unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("raisers/reject_tick.lua"),
        r#"return { type = "cron", interval = "60s", produces = "reject_tick" }"#,
    )
    .unwrap();
    fs::write(
        host.path().join("raisers/accept_tick.lua"),
        r#"return { type = "cron", interval = "60s", produces = "accept_tick" }"#,
    )
    .unwrap();
    fs::write(
        host.path().join("departments/reject_tick/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "reject_tick" } }
function M.pipeline(event)
  if event.payload.schema ~= "ideal-cron-fixture" then
    error("unknown-schema")
  end
end
return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("departments/accept_tick/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "accept_tick" }, produces = { "done" } }
function M.pipeline(event)
  assert(event.payload.raiser == "accept_tick", "expected real cron tick")
  raise("done", { seen = event.payload.raiser })
end
return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("tests/fire_raiser_test.lua"),
        r#"
local t = fkst.test

return {
  test_fire_raiser_surfaces_real_tick_rejection = function()
    local trace = t.fire_raiser("reject_tick")
    t.eq(trace.source_payload.raiser, "reject_tick")
    t.eq(trace.routed_to[1], "reject_tick")
    t.eq(trace.consumer_result.status, "error")
    t.is_true(string.find(trace.consumer_result.message, "unknown-schema", 1, true) ~= nil)
  end,

  test_fire_raiser_accepts_real_tick_and_captures_raises = function()
    local trace = t.fire_raiser("accept_tick")
    t.eq(trace.source_payload.raiser, "accept_tick")
    t.eq(trace.routed_to[1], "accept_tick")
    t.eq(trace.consumer_result.status, "accepted")
    t.eq(trace.raised[1].queue, "done")
    t.eq(trace.raised[1].payload.seen, "accept_tick")
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/fire_raiser_test.lua::test_fire_raiser_surfaces_real_tick_rejection"
        ),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/fire_raiser_test.lua::test_fire_raiser_accepts_real_tick_and_captures_raises"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn fire_raiser_uses_real_file_watch_fixture_payload() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/watch")).unwrap();
    fs::create_dir_all(host.path().join("raisers")).unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::create_dir_all(host.path().join("input")).unwrap();
    fs::write(host.path().join("input/ready.json"), "{}").unwrap();
    fs::write(
        host.path().join("raisers/files.lua"),
        r#"return { type = "file_watch", glob = "input/*.json", produces = "files" }"#,
    )
    .unwrap();
    fs::write(
        host.path().join("departments/watch/main.lua"),
        r#"
local M = {}
M.spec = { consumes = { "files" }, produces = { "done" } }
function M.pipeline(event)
  assert(string.find(event.payload.path, "/input/ready.json", 1, true) ~= nil, "expected file path payload")
  raise("done", { path = event.payload.path })
end
return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("tests/fire_file_watch_test.lua"),
        r#"
local t = fkst.test

return {
  test_file_watch_fixture_routes_real_path_payload = function()
    local trace = t.fire_raiser("files", { fixture = "input/ready.json" })
    t.eq(trace.consumer_result.status, "accepted")
    t.is_true(string.find(trace.source_payload.path, "/input/ready.json", 1, true) ~= nil)
    t.eq(trace.raised[1].payload.path, trace.source_payload.path)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/fire_file_watch_test.lua::test_file_watch_fixture_routes_real_path_payload"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_isolates_each_test_file_to_its_owner_root() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_a = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_b = tempfile::Builder::new().prefix("repo").tempdir().unwrap();

    for (package, label) in [(package_a.path(), "a"), (package_b.path(), "b")] {
        fs::create_dir_all(package.join("departments/probe")).unwrap();
        fs::create_dir_all(package.join("tests")).unwrap();
        fs::write(
            package.join("core.lua"),
            format!(r#"return {{ value = "{label}" }}"#),
        )
        .unwrap();
        fs::write(
            package.join("departments/probe/main.lua"),
            r#"
local M = {}
M.spec = { produces = { "seen" } }
local core = require("core")
function pipeline(event)
  raise("seen", { value = core.value, expected = event.payload.expected })
end
return M
"#,
        )
        .unwrap();
        fs::write(
            package.join("tests/owner_test.lua"),
            format!(
                r#"
local t = fkst.test
local core = require("core")
return {{
  test_require_core_uses_owner = function()
    t.eq(core.value, "{label}")
  end,
  test_run_department_uses_owner = function()
    local result = fkst.test.run_department("departments/probe/main.lua", {{ payload = {{ expected = "{label}" }} }})
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].payload.value, "{label}")
    t.eq(result.raises[1].payload.expected, "{label}")
  end,
}}
"#
            ),
        )
        .unwrap();
    }

    let output = run_lua_tests_with_packages(host.path(), &[package_a.path(), package_b.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert_eq!(
        out.matches("PASS tests/owner_test.lua::test_require_core_uses_owner")
            .count(),
        2,
        "stdout: {out}"
    );
    assert_eq!(
        out.matches("PASS tests/owner_test.lua::test_run_department_uses_owner")
            .count(),
        2,
        "stdout: {out}"
    );
    assert!(out.contains("4 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_host_department_uses_host_asset_with_package_graph_root() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new().prefix("repo").tempdir().unwrap();

    fs::create_dir_all(package.path()).unwrap();
    fs::create_dir_all(host.path().join("fkst")).unwrap();
    fs::write(
        host.path().join("fkst/standard_asset.lua"),
        r#"return { value = function() return "from-host" end }"#,
    )
    .unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    fs::write(
        host.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = { produces = { "seen" } }
local standard = require("fkst.standard_asset")
function pipeline(event)
  raise("seen", { value = standard.value() })
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/host_department_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_uses_host_standard_asset = function()
    local result = fkst.test.run_department("departments/probe/main.lua", { payload = {} })
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "host.seen")
    t.eq(result.raises[1].payload.value, "from-host")
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/host_department_test.lua::test_run_department_uses_host_standard_asset"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_delivers_namespaced_queue_for_bare_same_package_event() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("github-devloop")
        .tempdir()
        .unwrap();
    let owner_namespace = namespace(package.path());

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "tick" }},
  produces = {{ "seen" }},
}}
function pipeline(event)
  assert(event.queue == "{owner_namespace}.tick", "got " .. tostring(event.queue))
  raise("seen", {{ queue = event.queue }})
end
return M
"#
        ),
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/namespaced_event_queue_test.lua"),
        format!(
            r#"
local t = fkst.test
return {{
  test_run_department_delivers_namespaced_queue = function()
    local result = fkst.test.run_department(
      "departments/probe/main.lua",
      {{ queue = "tick", payload = {{}}, ts = 1 }}
    )
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "{owner_namespace}.seen")
    t.eq(result.raises[1].payload.queue, "{owner_namespace}.tick")
  end,
}}
"#
        ),
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/namespaced_event_queue_test.lua::test_run_department_delivers_namespaced_queue"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_preserves_cross_package_consumed_queue_event() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("github-devloop")
        .tempdir()
        .unwrap();
    let other = tempfile::Builder::new()
        .prefix("consensus")
        .tempdir()
        .unwrap();
    let owner_namespace = namespace(package.path());
    let other_namespace = namespace(other.path());

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "{other_namespace}.tick" }},
  produces = {{ "seen" }},
}}
function pipeline(event)
  assert(event.queue == "{other_namespace}.tick", "got " .. tostring(event.queue))
  raise("seen", {{ queue = event.queue }})
end
return M
"#
        ),
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/cross_package_event_queue_test.lua"),
        format!(
            r#"
local t = fkst.test
return {{
  test_run_department_preserves_cross_package_queue = function()
    local result = fkst.test.run_department(
      "departments/probe/main.lua",
      {{ queue = "{other_namespace}.tick", payload = {{}}, ts = 1 }}
    )
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "{owner_namespace}.seen")
    t.eq(result.raises[1].payload.queue, "{other_namespace}.tick")
  end,
}}
"#
        ),
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path(), other.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/cross_package_event_queue_test.lua::test_run_department_preserves_cross_package_queue"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_preserves_engine_failure_fact_queue_event() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("github-devloop")
        .tempdir()
        .unwrap();
    let owner_namespace = namespace(package.path());

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "fkst.failure_fact" },
  ephemeral = { "fkst.failure_fact" },
  produces = { "seen" },
}
function pipeline(event)
  assert(event.queue == "fkst.failure_fact", "got " .. tostring(event.queue))
  raise("seen", { queue = event.queue })
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/failure_fact_event_queue_test.lua"),
        format!(
            r#"
local t = fkst.test
return {{
  test_run_department_preserves_failure_fact_queue = function()
    local result = fkst.test.run_department(
      "departments/probe/main.lua",
      {{ queue = "fkst.failure_fact", payload = {{}}, ts = 1 }}
    )
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "{owner_namespace}.seen")
    t.eq(result.raises[1].payload.queue, "fkst.failure_fact")
  end,
}}
"#
        ),
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/failure_fact_event_queue_test.lua::test_run_department_preserves_failure_fact_queue"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_preserves_dead_letter_queue_event() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("github-devloop")
        .tempdir()
        .unwrap();
    let owner_namespace = namespace(package.path());
    let dead_letter_queue = format!("{owner_namespace}.dead_letter");

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "dead_letter" }},
  ephemeral = {{ "dead_letter" }},
  produces = {{ "seen" }},
}}
function pipeline(event)
  assert(event.queue == "{dead_letter_queue}", "got " .. tostring(event.queue))
  raise("seen", {{ queue = event.queue }})
end
return M
"#
        ),
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/dead_letter_event_queue_test.lua"),
        format!(
            r#"
local t = fkst.test
return {{
  test_run_department_preserves_dead_letter_queue = function()
    local result = fkst.test.run_department(
      "departments/probe/main.lua",
      {{ queue = "{dead_letter_queue}", payload = {{}}, ts = 1 }}
    )
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "{owner_namespace}.seen")
    t.eq(result.raises[1].payload.queue, "{dead_letter_queue}")
  end,
}}
"#
        ),
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/dead_letter_event_queue_test.lua::test_run_department_preserves_dead_letter_queue"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_legacy_flat_event_queue_stays_flat() {
    let package = tempfile::Builder::new()
        .prefix("github-devloop")
        .tempdir()
        .unwrap();

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "seen" },
}
function pipeline(event)
  assert(event.queue == "tick", "got " .. tostring(event.queue))
  raise("seen", { queue = event.queue })
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/legacy_flat_event_queue_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_keeps_legacy_flat_queue = function()
    local result = fkst.test.run_department(
      "departments/probe/main.lua",
      { queue = "tick", payload = {}, ts = 1 }
    )
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "seen")
    t.eq(result.raises[1].payload.queue, "tick")
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(package.path(), package.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/legacy_flat_event_queue_test.lua::test_run_department_keeps_legacy_flat_queue"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_records_declared_cross_package_raise_without_delivery() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("autochrono")
        .tempdir()
        .unwrap();

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}
function pipeline(event)
  raise("consensus.proposal", { source = event.payload.source })
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/cross_package_raise_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_records_declared_cross_package_raise = function()
    local result = fkst.test.run_department("departments/probe/main.lua", { payload = { source = "unit" } })
    t.eq(result.exit_code, 0)
    t.eq(#result.raises, 1)
    t.eq(result.raises[1].queue, "consensus.proposal")
    t.eq(result.raises[1].payload.source, "unit")
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/cross_package_raise_test.lua::test_run_department_records_declared_cross_package_raise"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_records_declared_cross_package_raise_in_legacy_flat() {
    let package = tempfile::Builder::new()
        .prefix("autochrono")
        .tempdir()
        .unwrap();

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}
function pipeline(event)
  raise("consensus.proposal", { source = event.payload.source })
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/legacy_flat_raise_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_records_declared_cross_package_raise = function()
    local result = fkst.test.run_department("departments/probe/main.lua", { payload = { source = "unit" } })
    t.eq(result.exit_code, 0)
    t.eq(#result.raises, 1)
    t.eq(result.raises[1].queue, "consensus.proposal")
    t.eq(result.raises[1].payload.source, "unit")
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(package.path(), package.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/legacy_flat_raise_test.lua::test_run_department_records_declared_cross_package_raise"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_department_rejects_undeclared_cross_package_raise() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("autochrono")
        .tempdir()
        .unwrap();

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}
function pipeline(event)
  raise("consensus.typo", {})
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/cross_package_raise_reject_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_rejects_undeclared_cross_package_raise = function()
    local result = fkst.test.run_department("departments/probe/main.lua", { payload = {} })
    t.eq(result.exit_code, 1)
    t.eq(#result.raises, 0)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/cross_package_raise_reject_test.lua::test_run_department_rejects_undeclared_cross_package_raise"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
    let err = stderr(&output);
    assert!(err.contains("unknown namespace"), "stderr: {err}");
}

#[test]
fn run_department_rejects_undeclared_cross_package_raise_in_legacy_flat() {
    let package = tempfile::Builder::new()
        .prefix("autochrono")
        .tempdir()
        .unwrap();

    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(
        package.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}
function pipeline(event)
  raise("consensus.typo", {})
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/legacy_flat_raise_reject_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_rejects_undeclared_cross_package_raise = function()
    local result = fkst.test.run_department("departments/probe/main.lua", { payload = {} })
    t.eq(result.exit_code, 1)
    t.eq(#result.raises, 0)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(package.path(), package.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/legacy_flat_raise_reject_test.lua::test_run_department_rejects_undeclared_cross_package_raise"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
    let err = stderr(&output);
    assert!(err.contains("legacy owner namespace"), "stderr: {err}");
}

#[test]
fn run_department_gates_cross_package_raise_allowlist_in_legacy_flat() {
    let package = tempfile::Builder::new()
        .prefix("autochrono")
        .tempdir()
        .unwrap();

    fs::write(
        package.path().join("core.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}
function pipeline(event)
  raise("consensus.proposal", {})
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/legacy_flat_non_department_raise_gate_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_gates_cross_package_raise_allowlist = function()
    local result = fkst.test.run_department("core.lua", { payload = {} })
    t.eq(result.exit_code, 1)
    t.eq(#result.raises, 0)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(package.path(), package.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/legacy_flat_non_department_raise_gate_test.lua::test_run_department_gates_cross_package_raise_allowlist"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
    let err = stderr(&output);
    assert!(err.contains("legacy owner namespace"), "stderr: {err}");
}

#[test]
fn run_department_rejects_cross_package_raise_from_non_department_subject() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("autochrono")
        .tempdir()
        .unwrap();

    fs::write(
        package.path().join("core.lua"),
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "consensus.proposal" },
}
function pipeline(event)
  raise("consensus.proposal", {})
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package
            .path()
            .join("tests/non_department_raise_gate_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_department_gates_cross_package_raise_allowlist_to_department_entrypoint = function()
    local result = fkst.test.run_department("core.lua", { payload = {} })
    t.eq(result.exit_code, 1)
    t.eq(#result.raises, 0)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests_with_packages(host.path(), &[package.path()]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/non_department_raise_gate_test.lua::test_run_department_gates_cross_package_raise_allowlist_to_department_entrypoint"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
    let err = stderr(&output);
    assert!(err.contains("unknown namespace"), "stderr: {err}");
}

#[test]
fn test_runner_continues_after_failure() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/failure_test.lua"),
        r#"
local t = fkst.test
return {
  test_a_pass = function() t.eq(1, 1) end,
  test_b_fail = function() t.eq(1, 2) end,
  test_c_pass = function() t.is_true(true) end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains("PASS tests/failure_test.lua::test_a_pass"),
        "stdout: {out}"
    );
    assert!(
        out.contains("FAIL tests/failure_test.lua::test_b_fail"),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/failure_test.lua::test_c_pass"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 1 failed"), "stdout: {out}");
}

#[test]
fn test_report_json_ignores_lua_forged_stdout_pass_lines() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/forgery_test.lua"),
        r#"
return {
  test_real = function()
    print("PASS forged::test_fake")
  end,
}
"#,
    )
    .unwrap();
    let report_path = host.path().join("report.json");

    let output = run_lua_tests_with_report(host.path(), host.path(), &report_path);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(out.contains("PASS forged::test_fake"), "stdout: {out}");
    assert!(
        out.contains("PASS tests/forgery_test.lua::test_real"),
        "stdout: {out}"
    );
    let report = read_report(&report_path);
    assert_eq!(report["schema"], "fkst.test.report.v1");
    assert_eq!(report["summary"]["passed"], 1);
    assert_eq!(report["summary"]["failed"], 0);
    let tests = report["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1, "report: {report}");
    assert_eq!(tests[0]["file"], "tests/forgery_test.lua");
    assert_eq!(tests[0]["name"], "test_real");
    assert_eq!(tests[0]["status"], "pass");
    assert!(
        tests[0].get("id").is_none(),
        "report entry must not expose ambiguous id: {report}"
    );
    assert!(
        !tests
            .iter()
            .any(|test| test["file"] == "forged" || test["name"] == "test_fake"),
        "report: {report}"
    );
}

#[test]
fn test_report_json_records_failing_test_before_exit_code_one() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/failing_report_test.lua"),
        r#"
local t = fkst.test
return {
  test_fails = function() t.eq("actual", "expected") end,
}
"#,
    )
    .unwrap();
    let report_path = host.path().join("report.json");

    let output = run_lua_tests_with_report(host.path(), host.path(), &report_path);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let report = read_report(&report_path);
    assert_eq!(report["summary"]["passed"], 0);
    assert_eq!(report["summary"]["failed"], 1);
    let tests = report["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 1, "report: {report}");
    assert!(
        tests[0].get("id").is_none(),
        "report entry must not expose ambiguous id: {report}"
    );
    assert_eq!(tests[0]["owner_namespace"], namespace(host.path()));
    assert_eq!(tests[0]["file"], "tests/failing_report_test.lua");
    assert_eq!(tests[0]["name"], "test_fails");
    assert_eq!(tests[0]["status"], "fail");
    assert!(
        tests[0]["error"]
            .as_str()
            .unwrap()
            .contains("expected \"expected\", got \"actual\""),
        "report: {report}"
    );
}

#[test]
fn test_report_json_disambiguates_same_relfile_across_package_roots() {
    let host = tempfile::Builder::new().prefix("host").tempdir().unwrap();
    let package_a = tempfile::Builder::new().prefix("pkg-a").tempdir().unwrap();
    let package_b = tempfile::Builder::new().prefix("pkg-b").tempdir().unwrap();
    for package in [package_a.path(), package_b.path()] {
        fs::create_dir_all(package.join("tests")).unwrap();
        fs::write(
            package.join("tests/same_test.lua"),
            r#"
return {
  test_same = function() end,
}
"#,
        )
        .unwrap();
    }
    let report_path = host.path().join("report.json");

    let output = run_lua_tests_with_packages_and_report(
        host.path(),
        &[package_a.path(), package_b.path()],
        &report_path,
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let report = read_report(&report_path);
    assert_eq!(report["summary"]["passed"], 2);
    assert_eq!(report["summary"]["failed"], 0);
    let tests = report["tests"].as_array().unwrap();
    assert_eq!(tests.len(), 2, "report: {report}");
    assert!(
        tests.iter().all(|test| test.get("id").is_none()),
        "report entries must not expose ambiguous ids: {report}"
    );
    let triples = tests
        .iter()
        .map(|test| {
            (
                test["owner_namespace"].as_str().unwrap().to_string(),
                test["file"].as_str().unwrap().to_string(),
                test["name"].as_str().unwrap().to_string(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(triples.len(), 2, "report: {report}");
    assert!(triples.contains(&(
        namespace(package_a.path()),
        "tests/same_test.lua".to_string(),
        "test_same".to_string()
    )));
    assert!(triples.contains(&(
        namespace(package_b.path()),
        "tests/same_test.lua".to_string(),
        "test_same".to_string()
    )));
}

#[test]
fn test_coverage_writes_json_and_lcov_for_production_lua_lines() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = { produces = { "done" } }
local helper = require("helper")

function pipeline(event)
  local value = helper.value(event.payload.value)
  local co = coroutine.create(function()
    helper.from_coroutine()
  end)
  local ok, err = coroutine.resume(co)
  assert(ok, err)
  raise("done", { value = value })
end
return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("helper.lua"),
        r#"
local M = {}

function M.value(value)
  local result = value .. "-covered"
  return result
end

function M.from_coroutine()
  local marker = "coroutine-covered"
  return marker
end

return M
"#,
    )
    .unwrap();
    fs::write(
        host.path().join("tests/coverage_test.lua"),
        r#"
local t = fkst.test

return {
  test_department = function()
    local result = fkst.test.run_department("departments/probe/main.lua", { payload = { value = "ok" } })
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].queue, "done")
    t.eq(result.raises[1].payload.value, "ok-covered")
  end,
}
"#,
    )
    .unwrap();
    let coverage_dir = host.path().join("coverage");

    let output = run_lua_tests_with_coverage(host.path(), host.path(), &coverage_dir);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let coverage = read_report(&coverage_dir.join("coverage.json"));
    assert!(
        coverage.get("departments/probe/main.lua").is_some(),
        "coverage: {coverage}"
    );
    assert!(coverage.get("helper.lua").is_some(), "coverage: {coverage}");
    assert!(
        coverage.get("tests/coverage_test.lua").is_none(),
        "test files must be excluded from production coverage: {coverage}"
    );
    let helper_lines = coverage["helper.lua"]["covered_lines"].as_array().unwrap();
    assert!(
        helper_lines.iter().any(|line| line.as_u64() == Some(10)),
        "coroutine-created lines must be covered: {coverage}"
    );
    let lcov = fs::read_to_string(coverage_dir.join("lcov.info")).unwrap();
    assert!(
        lcov.contains("SF:departments/probe/main.lua"),
        "lcov: {lcov}"
    );
    assert!(lcov.contains("SF:helper.lua"), "lcov: {lcov}");
    assert!(lcov.contains("DA:10,1"), "lcov: {lcov}");
    assert!(!lcov.contains("coverage_test.lua"), "lcov: {lcov}");
    assert!(!lcov.contains("BRDA:"), "lcov: {lcov}");
}

#[test]
fn test_runner_mocks_external_commands_fail_closed_and_isolates_tests() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::write(
        host.path().join("fkst.env"),
        "FKST_CANDIDATE_PREFIX=test-rc\nFKST_CANDIDATE_FROM_SEP=__base__\n",
    )
    .unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    fs::write(
        host.path().join("departments/probe/main.lua"),
        r#"
local M = {}
M.spec = { produces = { "seen" } }
function pipeline(event)
  local result = exec_sync(event.payload.cmd)
  raise("seen", { stdout = result.stdout, exit_code = result.exit_code })
end
return M
"#,
    )
    .unwrap();
    fs::create_dir_all(host.path().join("tests")).unwrap();
    fs::write(
        host.path().join("tests/mock_command_test.lua"),
        r#"
local t = fkst.test

return {
  test_01_exec_sync_uses_mock_and_records_call = function()
    t.mock_command("gh issue list", { stdout = "[{\"number\":7}]\n", exit_code = 0 })
    local result = exec_sync("gh issue list --json number")
    t.eq(result.stdout, "[{\"number\":7}]\n")
    t.eq(result.exit_code, 0)
    local calls = t.command_calls()
    t.eq(#calls, 1)
    t.eq(calls[1].rendered, "gh issue list --json number")
    t.eq(calls[1].program, "/bin/sh")
    t.eq(calls[1].args[1], "-c")
    t.eq(calls[1].args[2], "gh issue list --json number")
  end,

  test_02_codex_sync_uses_mock_and_records_prompt = function()
    t.mock_command("codex exec", { stdout = "draft", exit_code = 0 })
    local result = spawn_codex_sync({ prompt = "write draft" })
    t.eq(result.stdout, "draft")
    t.eq(result.stderr, "")
    t.eq(result.exit_code, 0)
    local calls = t.command_calls()
    t.eq(#calls, 1)
    t.eq(calls[1].program, "codex")
    t.eq(calls[1].stdin, "write draft")
    t.is_true(string.find(calls[1].rendered, "codex exec", 1, true) ~= nil)
    t.eq(calls[1].args[#calls[1].args], "-")
  end,

  test_03_codex_sync_nonzero_and_empty_stdout_pass_through = function()
    t.mock_command("codex exec", { stderr = "nope", exit_code = 12 })
    local result = spawn_codex_sync({ prompt = "fail draft" })
    t.eq(result.stdout, "")
    t.eq(result.stderr, "nope")
    t.eq(result.exit_code, 12)
  end,

  test_04_git_log_count_uses_mock_stdout = function()
    t.mock_command("git -C", { stdout = "a\nb\nc\n" })
    local count = git_log_count("topic", "1970-01-01T00:00:00Z")
    t.eq(count, 3)
    local calls = t.command_calls()
    t.is_true(string.find(calls[1].rendered, "git -C", 1, true) ~= nil)
    t.is_true(string.find(calls[1].rendered, "log", 1, true) ~= nil)
  end,

  test_05_git_read_primitives_parse_mocked_stdout = function()
    local worktree_stdout = table.concat({
      "worktree /repo",
      "HEAD aaaaaaaaaaaa",
      "",
      "worktree /repo/.fkst/runtime/worktrees/probe-1",
      "HEAD bbbbbbbbbbbb",
      "",
      "worktree /repo/.fkst/runtime/worktrees/probe-2",
      "HEAD cccccccccccc",
      "",
    }, "\n")
    t.mock_command("git -C", { stdout = worktree_stdout })
    t.eq(count_worktrees(), 2)

    t.mock_command("git -C", { stdout = "abc123\n\n def456 \n" })
    local shas = git_log_grep("topic", "1970-01-01T00:00:00Z")
    t.eq(#shas, 2)
    t.eq(shas[1], "abc123")
    t.eq(shas[2], "def456")

    local calls = t.command_calls()
    t.eq(#calls, 2)
    t.is_true(string.find(calls[1].rendered, "worktree list --porcelain", 1, true) ~= nil)
    t.is_true(string.find(calls[2].rendered, "--format=%H", 1, true) ~= nil)
  end,

  test_06_mock_commands_are_fifo_and_single_use = function()
    t.mock_command("git -C", { stdout = "first\n" })
    t.mock_command("git -C", { stdout = "second\nthird\n" })
    t.eq(git_log_count("topic", "now"), 1)
    t.eq(git_log_count("topic", "now"), 2)

    local ok, err = pcall(function()
      git_log_count("topic", "now")
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: git -C", 1, true) ~= nil)
  end,

  test_07_setup_worktree_fails_closed_when_later_git_call_is_unmocked = function()
    t.mock_command("git -C", { stdout = "main\n" })
    local ok, err = pcall(function()
      setup_worktree("mocked")
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: git -C", 1, true) ~= nil)

    local calls = t.command_calls()
    t.eq(#calls, 1)
    t.is_true(string.find(calls[1].rendered, "rev-parse --abbrev-ref HEAD", 1, true) ~= nil)
  end,

  test_08_unmocked_exec_sync_fails_closed = function()
    local ok, err = pcall(function()
      exec_sync("printf should-not-run")
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: printf should-not-run", 1, true) ~= nil)
  end,

  test_09_unmocked_codex_sync_fails_closed = function()
    local ok, err = pcall(function()
      spawn_codex_sync({ prompt = "unmocked" })
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: codex exec", 1, true) ~= nil)
  end,

  test_10_unmocked_git_fails_closed = function()
    local ok, err = pcall(function()
      git_log_count("topic", "now")
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: git -C", 1, true) ~= nil)
  end,

  test_11_spawn_codex_await_all_uses_mock = function()
    t.mock_command("codex exec", { stdout = "async draft", exit_code = 0 })
    local handle = spawn_codex({ prompt = "async prompt" })
    local results = await_all({ handle })
    t.eq(results[1].stdout, "async draft")
    t.eq(results[1].stderr, "")
    t.eq(results[1].exit_code, 0)
    local calls = t.command_calls()
    t.eq(calls[1].stdin, "async prompt")
  end,

  test_12_unmocked_spawn_codex_fails_on_await = function()
    local handle = spawn_codex({ prompt = "async unmocked" })
    local ok, err = pcall(function()
      await_all({ handle })
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: codex exec", 1, true) ~= nil)
  end,

  test_13_run_department_shares_mock_state = function()
    t.mock_command("gh issue list", { stdout = "dept\n", exit_code = 0 })
    local result = t.run_department("departments/probe/main.lua", { payload = { cmd = "gh issue list" } })
    t.eq(result.exit_code, 0)
    t.eq(result.raises[1].payload.stdout, "dept\n")
    local calls = t.command_calls()
    t.eq(#calls, 1)
    t.eq(calls[1].rendered, "gh issue list")
  end,

  test_14_per_test_isolation_clears_prior_mocks_and_calls = function()
    t.eq(#t.command_calls(), 0)
    local ok, err = pcall(function()
      exec_sync("gh issue list --json number")
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "unmocked external command: gh issue list --json number", 1, true) ~= nil)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(out.contains("14 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn test_runner_records_and_replays_external_command_cassettes() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("tests/cassettes")).unwrap();
    fs::write(
        host.path().join("tests/vcr_command_test.lua"),
        r#"
local t = fkst.test

return {
  test_01_record_writes_sanitized_cassette = function()
    t.with_command_cassette({
      path = "tests/cassettes/command.json",
      mode = "record",
      redact = {
        { value = "secret-token", replacement = "<TOKEN>" },
      },
    }, function()
      local result = exec_sync("printf secret-token")
      t.eq(result.stdout, "secret-token")
      t.eq(result.exit_code, 0)
    end)
    local calls = t.command_calls()
    t.eq(#calls, 1)
    t.eq(calls[1].rendered, "printf secret-token")
  end,

  test_02_replay_returns_cassette_without_live_command = function()
    t.with_command_cassette({
      path = "tests/cassettes/command.json",
      mode = "replay",
      redact = {
        { value = "secret-token", replacement = "<TOKEN>" },
      },
    }, function()
      local result = exec_sync("printf secret-token")
      t.eq(result.stdout, "<TOKEN>")
      t.eq(result.exit_code, 0)
    end)
    local calls = t.command_calls()
    t.eq(#calls, 1)
    t.eq(calls[1].stdout, "<TOKEN>")
  end,

  test_03_replay_mismatch_fails_closed = function()
    local ok, err = pcall(function()
      t.with_command_cassette({
        path = "tests/cassettes/command.json",
        mode = "replay",
      }, function()
        exec_sync("printf different")
      end)
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "VCR replay mismatch", 1, true) ~= nil)
  end,
}
"#,
    )
    .unwrap();

    let output = run_lua_tests(host.path(), host.path());

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(out.contains("3 passed, 0 failed"), "stdout: {out}");
    let cassette = fs::read_to_string(host.path().join("tests/cassettes/command.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&cassette).unwrap();
    assert_eq!(value["schema"], "fkst.test.command-cassette.v1");
    assert_eq!(value["entries"][0]["rendered"], "printf <TOKEN>");
    assert_eq!(value["entries"][0]["stdout"], "<TOKEN>");
    assert!(
        !cassette.contains("secret-token"),
        "cassette leaked secret: {cassette}"
    );
}

#[test]
fn test_surface_does_not_leak_to_production_run() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
function pipeline(event)
  assert(fkst == nil or fkst.test == nil, "test surface leaked to production")
end
"#,
    )
    .unwrap();

    write_single_package_workspace(host.path());
    let output = framework_command()
        .arg("run")
        .arg(&probe)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--owner-namespace")
        .arg(namespace(host.path()))
        .arg("--event")
        .arg("{}")
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
}

#[test]
fn production_run_does_not_require_from_host_cwd_when_owner_lacks_module() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    fs::write(host.path().join("core.lua"), r#"return { value = "host" }"#).unwrap();
    let probe = package.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "seen" } }
local core = require("core")
function pipeline(event)
  raise("seen", { value = core.value })
end
return M
"#,
    )
    .unwrap();

    write_workspace_for_roots(host.path(), &[package.path()]);
    let output = framework_command()
        .arg("run")
        .arg(&probe)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(package.path())
        .arg("--owner-namespace")
        .arg(namespace(package.path()))
        .arg("--event")
        .arg("{}")
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("require.denied"), "{err}");
    assert!(err.contains("not declared/visible"), "{err}");
}

#[test]
fn production_exec_sync_returns_typed_boundary_error_class() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "seen" } }
function pipeline(event)
  local result = exec_sync(event.payload.cmd)
  raise("seen", { exit_code = result.exit_code, error_class = result.error_class })
end
return M
"#,
    )
    .unwrap();

    write_single_package_workspace(host.path());
    let auth = framework_command()
        .arg("run")
        .arg(&probe)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--owner-namespace")
        .arg(namespace(host.path()))
        .arg("--event")
        .arg(r#"{"payload":{"cmd":"printf 'HTTP 401 bad credentials' >&2; exit 1"}}"#)
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert_eq!(
        auth.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&auth),
        stderr(&auth)
    );
    let raises = raised_entries(&auth);
    assert_eq!(raises[0]["payload"]["exit_code"], 1);
    assert_eq!(raises[0]["payload"]["error_class"], "auth-degraded");

    let throttle = framework_command()
        .arg("run")
        .arg(&probe)
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(host.path())
        .arg("--owner-namespace")
        .arg(namespace(host.path()))
        .arg("--event")
        .arg(r#"{"payload":{"cmd":"printf 'secondary rate limit' >&2; exit 1"}}"#)
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .output()
        .unwrap();

    assert_eq!(
        throttle.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&throttle),
        stderr(&throttle)
    );
    let raises = raised_entries(&throttle);
    assert_eq!(raises[0]["payload"]["exit_code"], 1);
    assert_eq!(raises[0]["payload"]["error_class"], "provider-throttle");
}

#[test]
fn run_accepts_host_owner_with_multiple_package_root_flags_as_graph_roots() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_a = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_b = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(host.path().join("departments/probe")).unwrap();
    fs::write(
        host.path().join("core.lua"),
        r#"return { value = "from-host" }"#,
    )
    .unwrap();
    fs::write(
        package_b.path().join("core.lua"),
        r#"return { value = "from-package-b" }"#,
    )
    .unwrap();
    let probe = host.path().join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "seen" } }
local core = require("core")
function pipeline(event)
  raise("seen", { value = core.value })
end
return M
"#,
    )
    .unwrap();

    write_workspace_for_roots(host.path(), &[package_a.path(), package_b.path()]);
    let output = run_command(host.path(), &probe)
        .arg("--package-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(package_a.path())
        .arg("--package-root")
        .arg(package_b.path())
        .arg("--owner-namespace")
        .arg("host")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let raises = raised_entries(&output);
    assert_eq!(raises[0]["payload"]["value"], "from-host");
}

#[test]
fn run_rejects_package_roots_env_even_with_singular_env() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    fs::create_dir_all(package.path().join("departments/probe")).unwrap();
    let probe = package.path().join("departments/probe/main.lua");
    fs::write(&probe, "function pipeline(event) end\n").unwrap();
    let joined = std::env::join_paths([package.path()]).unwrap();

    let output = run_command(host.path(), &probe)
        .env("FKST_PACKAGE_ROOTS", joined)
        .env("FKST_PACKAGE_ROOT", package.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("FKST_PACKAGE_ROOTS is not valid for `run`"),
        "{err}"
    );
}

#[test]
fn run_single_package_entrypoints_are_equivalent() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = host.path().join("packages/app");
    fs::create_dir_all(package.join("departments/probe")).unwrap();
    fs::write(package.join("core.lua"), r#"return { value = "owner" }"#).unwrap();
    let probe = package.join("departments/probe/main.lua");
    fs::write(
        &probe,
        r#"
local M = {}
M.spec = { produces = { "seen" } }
local core = require("core")
function pipeline(event)
  raise("seen", { core = core.value, input = event.payload.value })
end
return M
"#,
    )
    .unwrap();

    write_workspace_for_roots(host.path(), &[&package]);
    let flag = run_command(host.path(), &probe)
        .arg("--package-root")
        .arg(&package)
        .arg("--owner-namespace")
        .arg(namespace(&package))
        .output()
        .unwrap();
    let singular = run_command(host.path(), &probe)
        .arg("--owner-namespace")
        .arg(namespace(&package))
        .env("FKST_PACKAGE_ROOT", &package)
        .output()
        .unwrap();
    let package_is_host = run_command(&package, &probe)
        .arg("--package-root")
        .arg(&package)
        .arg("--owner-namespace")
        .arg(namespace(&package))
        .output()
        .unwrap();

    for output in [&flag, &singular, &package_is_host] {
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            stdout(output),
            stderr(output)
        );
    }
    assert_eq!(stdout(&flag), stdout(&singular));
    assert_ne!(stdout(&flag), stdout(&package_is_host));
    assert!(
        stdout(&package_is_host).contains("RAISED: W3sicXVldWUiOiJzZWVuIiw"),
        "stdout: {}",
        stdout(&package_is_host)
    );
}
