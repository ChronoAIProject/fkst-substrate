use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod support;

use support::manifest_fixture::write_workspace_for_roots;

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn framework_command() -> Command {
    let mut command = Command::new(framework_bin());
    command.env_remove("FKST_SUPERVISOR_PID");
    command
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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
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
fn run_graph_drives_reliable_multi_hop_cross_package_flow_to_quiescence() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package_a = tempfile::Builder::new().prefix("alpha").tempdir().unwrap();
    let package_b = tempfile::Builder::new().prefix("bravo").tempdir().unwrap();
    let package_c = tempfile::Builder::new()
        .prefix("charlie")
        .tempdir()
        .unwrap();
    let ns_a = namespace(package_a.path());
    let ns_b = namespace(package_b.path());
    let ns_c = namespace(package_c.path());

    write_department(
        package_a.path(),
        "first",
        &format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "start" }},
  produces = {{ "{ns_b}.middle" }},
}}
function M.pipeline(event)
  assert(event.queue == "{ns_a}.start", "got " .. tostring(event.queue))
  raise("{ns_b}.middle", {{ from = "first", seed = event.payload.seed }})
end
return M
"#
        ),
    );
    write_department(
        package_b.path(),
        "second",
        &format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "middle" }},
  published_seam = {{ "middle" }},
  produces = {{ "{ns_c}.finish" }},
}}
function M.pipeline(event)
  assert(event.queue == "{ns_b}.middle", "got " .. tostring(event.queue))
  raise("{ns_c}.finish", {{ from = "second", seed = event.payload.seed }})
end
return M
"#
        ),
    );
    write_department(
        package_c.path(),
        "third",
        &format!(
            r#"
local M = {{}}
M.spec = {{
  consumes = {{ "finish" }},
  published_seam = {{ "finish" }},
  produces = {{}},
}}
function M.pipeline(event)
  assert(event.queue == "{ns_c}.finish", "got " .. tostring(event.queue))
  assert(event.payload.from == "second", "expected second hop")
end
return M
"#
        ),
    );

    fs::create_dir_all(package_a.path().join("tests")).unwrap();
    fs::write(
        package_a.path().join("tests/run_graph_test.lua"),
        format!(
            r#"
local t = fkst.test

local function stable(trace)
  local rows = {{}}
  for i, step in ipairs(trace.steps) do
    local raises = {{}}
    for j, raised in ipairs(step.raises) do
      raises[j] = raised.queue .. ":" .. raised.payload.from .. ":" .. tostring(raised.payload.seed)
    end
    rows[i] = step.delivery_id .. ">" .. step.queue .. ">" .. step.consumer .. ">" .. step.status .. ">" .. table.concat(raises, ",")
  end
  return table.concat(rows, "|")
end

return {{
  test_run_graph_drives_multi_hop_to_quiescence = function()
    local trace = t.run_graph({{
      queue = "start",
      payload = {{ seed = "s1" }},
      source_ref = {{ kind = "external", reference = "unit/multi-hop" }},
    }}, {{ max_steps = 8 }})

    t.eq(trace.status, "quiescent")
    t.eq(#trace.steps, 3)
    t.eq(trace.steps[1].queue, "{ns_a}.start")
    t.eq(trace.steps[1].consumer, "{ns_a}.first")
    t.eq(trace.steps[1].raises[1].queue, "{ns_b}.middle")
    t.eq(trace.steps[2].queue, "{ns_b}.middle")
    t.eq(trace.steps[2].consumer, "{ns_b}.second")
    t.eq(trace.steps[2].raises[1].queue, "{ns_c}.finish")
    t.eq(trace.steps[3].queue, "{ns_c}.finish")
    t.eq(trace.steps[3].consumer, "{ns_c}.third")
    t.eq(#trace.steps[3].raises, 0)
  end,

  test_run_graph_trace_is_deterministic = function()
    local event = {{
      queue = "start",
      payload = {{ seed = "s2" }},
      source_ref = {{ kind = "external", reference = "unit/deterministic" }},
    }}
    local first = t.run_graph(event, {{ max_steps = 8 }})
    local second = t.run_graph(event, {{ max_steps = 8 }})

    t.eq(stable(first), stable(second))
    t.eq(first.final.pending, 0)
    t.eq(second.final.pending, 0)
  end,
}}
"#
        ),
    )
    .unwrap();

    let output = run_lua_tests_with_packages(
        host.path(),
        &[package_a.path(), package_b.path(), package_c.path()],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/run_graph_test.lua::test_run_graph_drives_multi_hop_to_quiescence"
        ),
        "stdout: {out}"
    );
    assert!(
        out.contains("PASS tests/run_graph_test.lua::test_run_graph_trace_is_deterministic"),
        "stdout: {out}"
    );
    assert!(out.contains("2 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_graph_isolates_codex_logs_per_run_and_preserves_intra_graph_visibility() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("codexlog")
        .tempdir()
        .unwrap();
    let ns = namespace(package.path());

    write_department(
        package.path(),
        "worker",
        r#"
local M = {}
M.spec = {
  consumes = { "start" },
  produces = { "observed" },
}

local function matching_run_exists()
  local runs = fkst.codex_runs()
  for _, group in ipairs({ runs.running, runs.recent }) do
    for _, run in ipairs(group) do
      if run.dedup_key == "shared-exec-ref" then
        return true
      end
    end
  end
  return false
end

function M.pipeline(event)
  if matching_run_exists() then
    raise("observed", { spawned = false })
    return
  end

  local result = spawn_codex_sync({
    prompt = "graph codex work",
    dedup_key = "shared-exec-ref",
  })
  assert(result.exit_code == 0, "expected mocked codex success")
  raise("observed", { spawned = true })
end
return M
"#,
    );
    write_department(
        package.path(),
        "observer",
        r#"
local M = {}
M.spec = {
  consumes = { "observed" },
  produces = {},
}

local function matching_run_exists()
  local runs = fkst.codex_runs()
  for _, group in ipairs({ runs.running, runs.recent }) do
    for _, run in ipairs(group) do
      if run.dedup_key == "shared-exec-ref" then
        return true
      end
    end
  end
  return false
end

function M.pipeline(event)
  assert(event.payload.spawned, "ambient codex status suppressed in-graph spawn")
  assert(matching_run_exists(), "in-graph codex status was not visible to a later step")
end
return M
"#,
    );
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/run_graph_codex_log_test.lua"),
        r#"
local t = fkst.test

local function matching_run_exists()
  local runs = fkst.codex_runs()
  for _, group in ipairs({ runs.running, runs.recent }) do
    for _, run in ipairs(group) do
      if run.dedup_key == "shared-exec-ref" then
        return true
      end
    end
  end
  return false
end

return {
  test_run_graph_codex_logs_are_hermetic_per_run = function()
    t.mock_command("codex exec", { stdout = "ambient", exit_code = 0 })
    local ambient = spawn_codex_sync({
      prompt = "ambient codex work",
      dedup_key = "shared-exec-ref",
    })
    t.eq(ambient.exit_code, 0)
    t.is_true(matching_run_exists(), "ambient codex status seed was not visible")

    t.mock_command("codex exec", { stdout = "first graph", exit_code = 0 })
    t.mock_command("codex exec", { stdout = "second graph", exit_code = 0 })
    local event = {
      queue = "PLACEHOLDER.start",
      payload = {},
      source_ref = { kind = "external", reference = "unit/codex-log" },
    }
    local first = t.run_graph(event, { max_steps = 2 })
    local second = t.run_graph(event, { max_steps = 2 })

    t.eq(first.status, "quiescent")
    t.eq(second.status, "quiescent")
    t.eq(#first.steps, 2)
    t.eq(#second.steps, 2)
    t.eq(first.steps[2].status, "accepted", first.steps[2].error)
    t.eq(second.steps[2].status, "accepted", second.steps[2].error)

    local calls = t.command_calls()
    t.eq(#calls, 3)
    t.eq(calls[1].stdin, "ambient codex work")
    t.eq(calls[2].stdin, "graph codex work")
    t.eq(calls[3].stdin, "graph codex work")
  end,
}
"#
        .replace("PLACEHOLDER", &ns),
    )
    .unwrap();

    write_workspace_for_roots(host.path(), &[package.path()]);
    let mut cmd = framework_command();
    let output = cmd
        .arg("test")
        .arg("--project-root")
        .arg(host.path())
        .arg("--package-root")
        .arg(package.path())
        .current_dir(host.path())
        .env("FKST_RUNTIME_ROOT", host.path().join(".fkst/runtime"))
        .env("FKST_RUNTIME_LOG_DIR", host.path().join("ambient-logs"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(
            "PASS tests/run_graph_codex_log_test.lua::test_run_graph_codex_logs_are_hermetic_per_run"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_graph_step_cap_fails_loudly() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("looping")
        .tempdir()
        .unwrap();
    let ns = namespace(package.path());

    write_department(
        package.path(),
        "ping",
        r#"
local M = {}
M.spec = {
  consumes = { "ping" },
  produces = { "pong" },
}
function M.pipeline(event)
  local next = (event.payload.count or 0) + 1
  raise("pong", {
    count = next,
    source_ref = { kind = "external", reference = "unit/cap/" .. tostring(next) },
  })
end
return M
"#,
    );
    write_department(
        package.path(),
        "pong",
        r#"
local M = {}
M.spec = {
  consumes = { "pong" },
  produces = { "ping" },
}
function M.pipeline(event)
  local next = (event.payload.count or 0) + 1
  raise("ping", {
    count = next,
    source_ref = { kind = "external", reference = "unit/cap/" .. tostring(next) },
  })
end
return M
"#,
    );
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/run_graph_cap_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_graph_cap_raises = function()
    local ok, err = pcall(function()
      t.run_graph({
        queue = "PLACEHOLDER.ping",
        payload = { count = 0 },
        source_ref = { kind = "external", reference = "unit/cap" },
      }, { max_deliveries = 2 })
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "max_steps", 1, true) ~= nil, tostring(err))
  end,
}
"#
        .replace("PLACEHOLDER", &ns),
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
        out.contains("PASS tests/run_graph_cap_test.lua::test_run_graph_cap_raises"),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_graph_exact_step_cap_returns_quiescent_when_no_pending_deliveries_remain() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("exactcap")
        .tempdir()
        .unwrap();
    let ns = namespace(package.path());

    write_department(
        package.path(),
        "first",
        r#"
local M = {}
M.spec = {
  consumes = { "start" },
  produces = { "done" },
}
function M.pipeline(event)
  raise("done", { seed = event.payload.seed })
end
return M
"#,
    );
    write_department(
        package.path(),
        "done",
        r#"
local M = {}
M.spec = {
  consumes = { "done" },
  produces = {},
}
function M.pipeline(event)
  assert(event.payload.seed == "exact", "expected exact seed")
end
return M
"#,
    );
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/run_graph_exact_cap_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_graph_exact_step_cap_returns_quiescent = function()
    local trace = t.run_graph({
      queue = "PLACEHOLDER.start",
      payload = { seed = "exact" },
      source_ref = { kind = "external", reference = "unit/exact-cap" },
    }, { max_steps = 2 })
    t.eq(trace.status, "quiescent")
    t.eq(#trace.steps, 2)
    t.eq(trace.final.pending, 0)
  end,
}
"#
        .replace("PLACEHOLDER", &ns),
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
            "PASS tests/run_graph_exact_cap_test.lua::test_run_graph_exact_step_cap_returns_quiescent"
        ),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_graph_can_fire_declared_source_to_start_graph() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new().prefix("source").tempdir().unwrap();
    let ns = namespace(package.path());

    fs::create_dir_all(package.path().join("raisers")).unwrap();
    fs::write(
        package.path().join("raisers/tick.lua"),
        r#"return { type = "cron", interval = "60s", produces = "tick" }"#,
    )
    .unwrap();
    write_department(
        package.path(),
        "tick",
        r#"
local M = {}
M.spec = {
  consumes = { "tick" },
  produces = { "done" },
}
function M.pipeline(event)
  raise("done", { raiser = event.payload.raiser })
end
return M
"#,
    );
    write_department(
        package.path(),
        "done",
        r#"
local M = {}
M.spec = {
  consumes = { "done" },
  produces = {},
}
function M.pipeline(event)
  assert(event.payload.raiser ~= nil, "expected cron payload")
end
return M
"#,
    );
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/run_graph_source_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_graph_fires_cron_source = function()
    local trace = t.run_graph("PLACEHOLDER.tick", { max_steps = 4 })
    t.eq(trace.status, "quiescent")
    t.eq(#trace.steps, 2)
    t.eq(trace.steps[1].queue, "PLACEHOLDER.tick")
    t.eq(trace.steps[1].raises[1].queue, "PLACEHOLDER.done")
    t.eq(trace.steps[1].raises[1].payload.raiser, "PLACEHOLDER.tick")
    t.eq(trace.steps[2].queue, "PLACEHOLDER.done")
  end,
}
"#
        .replace("PLACEHOLDER", &ns),
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
        out.contains("PASS tests/run_graph_source_test.lua::test_run_graph_fires_cron_source"),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

#[test]
fn run_graph_uses_reliable_delivery_source_ref_enforcement() {
    let host = tempfile::Builder::new().prefix("repo").tempdir().unwrap();
    let package = tempfile::Builder::new()
        .prefix("reliable")
        .tempdir()
        .unwrap();
    let ns = namespace(package.path());

    write_department(
        package.path(),
        "probe",
        r#"
local M = {}
M.spec = {
  consumes = { "jobs" },
  produces = {},
}
function M.pipeline(event)
end
return M
"#,
    );
    fs::create_dir_all(package.path().join("tests")).unwrap();
    fs::write(
        package.path().join("tests/run_graph_source_ref_test.lua"),
        r#"
local t = fkst.test
return {
  test_run_graph_requires_source_ref_for_reliable_delivery = function()
    local ok, err = pcall(function()
      t.run_graph({ queue = "PLACEHOLDER.jobs", payload = {} }, { max_steps = 1 })
    end)
    t.eq(ok, false)
    t.is_true(string.find(tostring(err), "requires source_ref", 1, true) ~= nil, tostring(err))
  end,
}
"#
        .replace("PLACEHOLDER", &ns),
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
        out.contains("PASS tests/run_graph_source_ref_test.lua::test_run_graph_requires_source_ref_for_reliable_delivery"),
        "stdout: {out}"
    );
    assert!(out.contains("1 passed, 0 failed"), "stdout: {out}");
}

fn write_department(root: &Path, name: &str, body: &str) {
    let path: PathBuf = root.join("departments").join(name).join("main.lua");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}
