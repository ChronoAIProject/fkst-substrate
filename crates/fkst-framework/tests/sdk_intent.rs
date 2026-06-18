// path-based integration tests own behavior coverage while runtime modules keep runtime code.

#[path = "../src/sdk_intent.rs"]
mod sdk_intent;
mod support;

use mlua::Lua;
use std::path::Path;
use support::process_sandbox::ProcessSandbox;
use tempfile::tempdir;

fn in_sandbox<T>(
    dir: &Path,
    configure: impl FnOnce(&mut ProcessSandbox),
    f: impl FnOnce() -> T,
) -> T {
    let mut sandbox = ProcessSandbox::new();
    sandbox.enter_cwd(dir);
    configure(&mut sandbox);
    sandbox.run(f)
}

fn register(lua: &Lua) {
    sdk_intent::register(lua).unwrap();
}

#[test]
fn intent_declare_and_visible_result_roundtrip() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let durable = tempdir().unwrap();
    register(&lua);

    let (intent_id, result_id, next_id): (String, String, String) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.durable_root(durable.path());
        },
        || {
            lua.load(
                r#"
                local intent = declare_intent("ready-implementing", "issue-133", "codex", "issue/133/implement")
                local visible = wait_until_intent_visible(intent.intent_id)
                local marker = write_result_marker(visible.intent_id, { run_id = "run-1" })
                local result = wait_until_result_visible(marker.result_id)
                local next = derive_next_transition_from_visible_result(marker.result_id)
                return visible.intent_id, result.result_id, next.run_id
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert!(intent_id.starts_with("ready-implementing/issue-133/codex/"));
    assert_eq!(result_id, format!("{intent_id}:result"));
    assert_eq!(next_id, "run-1");
    assert!(durable.path().join("intent.redb").exists());
}

#[test]
fn missing_durable_root_fails_closed_before_declaring_intent() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    register(&lua);

    let err = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.unset_env(fkst_common::durable_layout::DURABLE_ROOT_ENV);
        },
        || {
            lua.load(
                r#"
                return declare_intent("edge", "generation", "codex", "effect-key")
                "#,
            )
            .eval::<()>()
            .unwrap_err()
        },
    );

    assert!(err.to_string().contains("FKST_DURABLE_ROOT must be set"));
}

#[test]
fn perform_or_recover_uses_recovery_before_non_idempotent_effect() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let durable = tempdir().unwrap();
    register(&lua);

    let (result, recovered_calls, performed_calls): (String, i64, i64) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.durable_root(durable.path());
        },
        || {
            lua.load(
                r#"
                local intent = declare_intent("edge", "generation", "github-comment", "comment-key")
                local recovered_calls = 0
                local performed_calls = 0
                local result = perform_or_recover_effect(intent.intent_id, "comment-key",
                    function(effect_key)
                        recovered_calls = recovered_calls + 1
                        return { comment_id = "already-visible", effect_key = effect_key }
                    end,
                    function(_effect_key)
                        performed_calls = performed_calls + 1
                        return { comment_id = "new" }
                    end)
                return result.comment_id, recovered_calls, performed_calls
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(result, "already-visible");
    assert_eq!(recovered_calls, 1);
    assert_eq!(performed_calls, 0);
}

#[test]
fn perform_or_recover_persists_result_and_skips_duplicate_perform() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let durable = tempdir().unwrap();
    register(&lua);

    let (first, second, performed_calls): (String, String, i64) = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.durable_root(durable.path());
        },
        || {
            lua.load(
                r#"
                local intent = declare_intent("edge", "generation", "codex", "run-key")
                local performed_calls = 0
                local function recover(_effect_key)
                    return nil
                end
                local function perform(effect_key)
                    performed_calls = performed_calls + 1
                    return { run_id = "run-1", effect_key = effect_key }
                end
                local first = perform_or_recover_effect(intent.intent_id, "run-key", recover, perform)
                local second = perform_or_recover_effect(intent.intent_id, "run-key", recover, perform)
                return first.run_id, second.run_id, performed_calls
                "#,
            )
            .eval()
            .unwrap()
        },
    );

    assert_eq!(first, "run-1");
    assert_eq!(second, "run-1");
    assert_eq!(performed_calls, 1);
}

#[test]
fn same_effect_key_cannot_be_rebound_to_another_intent() {
    let lua = Lua::new();
    let host = tempdir().unwrap();
    let durable = tempdir().unwrap();
    register(&lua);

    let err = in_sandbox(
        host.path(),
        |sandbox| {
            sandbox.durable_root(durable.path());
        },
        || {
            lua.load(
                r#"
                declare_intent("edge-a", "generation", "codex", "same-key")
                declare_intent("edge-b", "generation", "codex", "same-key")
                "#,
            )
            .exec()
            .unwrap_err()
        },
    );

    assert!(err.to_string().contains("already belongs to intent"));
}
