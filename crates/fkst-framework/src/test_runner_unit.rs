use super::external_command::{MockCommandResult, MockCommandState};
use super::path_resolver::PackageRoots;
use super::test_runner::{run_department, TestRunCache, TestRunCacheStats};
use mlua::{LuaSerdeExt, Table, Value};
use std::path::Path;
use tempfile::TempDir;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn write_package_manifest(root: &Path) {
    write(
        &root.join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["."]
"#,
    );
    write(
        &root.join("fkst.toml"),
        r#"
kind = "package"
name = "pkg"

[code]
root = "."
"#,
    );
}

fn table_len(table: Table) -> usize {
    table.pairs::<Value, Value>().count()
}

#[test]
fn run_department_cache_reuses_derivations_and_preserves_isolation() {
    let temp = TempDir::new().unwrap();
    write_package_manifest(temp.path());
    let dept_dir = temp.path().join("departments/worker");
    std::fs::create_dir_all(&dept_dir).unwrap();
    std::fs::write(
        temp.path().join("helper.lua"),
        r#"
        local M = {}
        local counter = 0
        function M.next()
            counter = counter + 1
            return counter
        end
        return M
        "#,
    )
    .unwrap();
    let main = dept_dir.join("main.lua");
    std::fs::write(
        &main,
        r#"
        function pipeline(event)
            local helper = require("helper")
            local result = exec_sync({ cmd = "echo " .. tostring(event.n) })
            raise("pkg.done", { stdout = result.stdout, n = event.n, counter = helper.next() })
        end
        return {
            spec = { produces = { "pkg.done" } },
            pipeline = pipeline,
        }
        "#,
    )
    .unwrap();
    let roots = PackageRoots::resolve(temp.path(), vec![temp.path().to_path_buf()]).unwrap();
    let owner_root = temp.path().canonicalize().unwrap();
    let owner_namespace = roots.sole_package_namespace().unwrap().to_string();
    let cache = TestRunCache::new(roots.clone()).unwrap();
    let mock_commands = MockCommandState::new();
    let outer_lua = crate::mlua_init::new_lua();

    let first_event = outer_lua
        .to_value(&serde_json::json!({"queue": "jobs", "payload": {}, "ts": 1, "n": 1}))
        .unwrap();
    mock_commands
        .push_mock(
            "echo 1".to_string(),
            MockCommandResult {
                stdout: "one\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
            },
        )
        .unwrap();
    let first = run_department(
        &outer_lua,
        &cache,
        &roots,
        &owner_root,
        &owner_namespace,
        mock_commands.clone(),
        "departments/worker/main.lua".to_string(),
        first_event,
        None,
        None,
    )
    .unwrap();
    assert_eq!(first.get::<i64>("exit_code").unwrap(), 0);
    let first_raises: Table = first.get("raises").unwrap();
    let first_raise: Table = first_raises.get(1).unwrap();
    let first_payload: Table = first_raise.get("payload").unwrap();
    assert_eq!(first_payload.get::<i64>("counter").unwrap(), 1);
    assert_eq!(table_len(first_raises), 1);
    assert_eq!(mock_commands.calls().unwrap().len(), 1);

    mock_commands.reset().unwrap();
    let second_event = outer_lua
        .to_value(&serde_json::json!({"queue": "jobs", "payload": {}, "ts": 2, "n": 2}))
        .unwrap();
    mock_commands
        .push_mock(
            "echo 2".to_string(),
            MockCommandResult {
                stdout: "two\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
            },
        )
        .unwrap();
    let second = run_department(
        &outer_lua,
        &cache,
        &roots,
        &owner_root,
        &owner_namespace,
        mock_commands.clone(),
        "departments/worker/main.lua".to_string(),
        second_event,
        None,
        None,
    )
    .unwrap();

    assert_eq!(second.get::<i64>("exit_code").unwrap(), 0);
    let second_raises: Table = second.get("raises").unwrap();
    let second_raise: Table = second_raises.get(1).unwrap();
    let second_payload: Table = second_raise.get("payload").unwrap();
    assert_eq!(second_payload.get::<i64>("counter").unwrap(), 1);
    assert_eq!(table_len(second_raises), 1);
    assert_eq!(mock_commands.calls().unwrap().len(), 1);
    assert_eq!(
        cache.stats(),
        TestRunCacheStats {
            owner_unit_misses: 1,
            declared_produces_misses: 1,
            declared_consumes_misses: 1,
        }
    );
    assert_eq!(cache.lua_chunk_cache().chunk_count(), 1);
}
