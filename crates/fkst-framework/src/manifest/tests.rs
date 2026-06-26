use super::*;
use std::fs;
use std::path::Path;

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn write_workspace(root: &Path) {
    write(
        &root.join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["packages/*", "libraries/*"]
"#,
    );
}

fn write_workspace_by_kind(root: &Path) {
    fs::create_dir_all(root.join("libraries")).unwrap();
    write(
        &root.join("fkst.workspace.toml"),
        r#"
[workspace]
packages = ["packages/*"]
libraries = ["std", "libraries/*"]
"#,
    );
}

fn write_package(root: &Path, name: &str, libs: &[&str]) {
    let deps = libs
        .iter()
        .map(|lib| format!(r#""{lib}""#))
        .collect::<Vec<_>>()
        .join(", ");
    write(
        &root.join(format!("packages/{name}/fkst.toml")),
        &format!(
            r#"
kind = "package"
name = "{name}"
persistence_class = "stateless_adapter"

[code]
root = "."

[lib_deps]
libraries = [{deps}]
"#
        ),
    );
}

fn write_library(root: &Path, name: &str) {
    write(
        &root.join(format!("libraries/{name}/fkst.toml")),
        &format!(
            r#"
kind = "library"
name = "{name}"

[code]
root = "."

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"
"#
        ),
    );
}

fn write_library_allowing(root: &Path, name: &str, allow: &[&str]) {
    let allow = allow
        .iter()
        .map(|unit| format!(r#""{unit}""#))
        .collect::<Vec<_>>()
        .join(", ");
    write(
        &root.join(format!("libraries/{name}/fkst.toml")),
        &format!(
            r#"
kind = "library"
name = "{name}"

[code]
root = "."

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"

[visibility]
allow = [{allow}]
"#
        ),
    );
}

fn write_library_with_dependency_constraints(
    root: &Path,
    name: &str,
    deps: &[&str],
    allowed: Option<&[&str]>,
) {
    let deps = deps
        .iter()
        .map(|library| format!(r#""{library}""#))
        .collect::<Vec<_>>()
        .join(", ");
    let constraints = allowed
        .map(|allowed| {
            let allowed = allowed
                .iter()
                .map(|library| format!(r#""{library}""#))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                r#"
[dependency_constraints]
allowed_lib_deps = [{allowed}]
"#
            )
        })
        .unwrap_or_default();
    write(
        &root.join(format!("libraries/{name}/fkst.toml")),
        &format!(
            r#"
kind = "library"
name = "{name}"

[code]
root = "."

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"

[lib_deps]
libraries = [{deps}]
{constraints}"#
        ),
    );
}

#[test]
fn parses_workspace_unit_lists_and_lock_manifests() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.workspace.toml"),
        r#"
[workspace]
packages = ["packages/*"]
libraries = ["std", "libraries/*"]

[registries]
workspace = "workspace"
"#,
    );
    write(
        &temp.path().join("fkst.lock"),
        r#"
[[package]]
name = "std"
source = "workspace"
version = "0.1.0"
"#,
    );
    write(
        &temp.path().join("packages/app/fkst.toml"),
        r#"
kind = "package"
name = "app"
persistence_class = "stateless_adapter"

[code]
root = "."

[lib_deps]
libraries = ["std"]

[event_deps]
packages = ["host"]
"#,
    );

    let workspace =
        WorkspaceManifest::parse_file(&temp.path().join("fkst.workspace.toml")).unwrap();
    let lockfile = Lockfile::parse_file(&temp.path().join("fkst.lock")).unwrap();
    let unit = UnitManifest::parse_file(&temp.path().join("packages/app/fkst.toml")).unwrap();

    assert_eq!(
        workspace.discovered_units(),
        &["packages/*", "std", "libraries/*"]
    );
    assert_eq!(
        workspace.registries().get("workspace").unwrap(),
        "workspace"
    );
    assert_eq!(lockfile.entries().len(), 1);
    assert_eq!(unit.kind, UnitKind::Package(PackageKind::Flat));
    assert_eq!(unit.name, "app");
    assert_eq!(
        unit.persistence_class(),
        Some(PersistenceClass::StatelessAdapter)
    );
    assert_eq!(unit.code_root, Path::new("."));
    assert_eq!(unit.lib_deps, vec![LibDep::new("std")]);
    assert_eq!(unit.event_deps, vec![EventDep::new("host")]);
}

#[test]
fn parses_legacy_workspace_units() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());

    let workspace =
        WorkspaceManifest::parse_file(&temp.path().join("fkst.workspace.toml")).unwrap();

    assert_eq!(workspace.discovered_units(), &["packages/*", "libraries/*"]);
}

#[test]
fn catalog_indexes_owner_and_declared_public_library_modules() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &["std"]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library(temp.path(), "std");
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/std/private/secret.lua"),
        "return {}\n",
    );

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let scope = catalog.require_scope_for_unit("app").unwrap();

    assert!(scope.resolve("main").is_some());
    assert!(scope.resolve("std.fkst.json").is_some());
    assert!(scope.resolve("fkst.json").is_none());
    assert!(scope.resolve("secret").is_none());
}

#[test]
fn library_publishable_defaults_false_and_does_not_change_intra_repo_visibility() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &["std"]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library(temp.path(), "std");
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let scope = catalog.require_scope_for_unit("app").unwrap();
    let std = catalog.units().find(|unit| unit.name() == "std").unwrap();

    assert!(!std.manifest().library.as_ref().unwrap().publishable);
    assert!(scope.resolve("std.fkst.json").is_some());
}

#[test]
fn library_publishable_parses_true() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"
publishable = true
"#,
    );

    let manifest = UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap();

    assert!(manifest.library.as_ref().unwrap().publishable);
}

#[test]
fn package_manifest_rejects_library_section() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "package"
name = "app"
persistence_class = "stateless_adapter"

[code]
root = "."

[library]
name = "app"
stable_id = "app"
version = "0.1.0"
publishable = true
"#,
    );

    let err = UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap_err();
    let msg = format!("{err:#}");

    assert!(
        msg.contains("package manifest must not declare `[library]`"),
        "{msg}"
    );
}

#[test]
fn library_manifest_rejects_unknown_library_field() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"
unexpected = true
"#,
    );

    let err = UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("unknown field `unexpected`"), "{msg}");
}

#[test]
fn catalog_prefixes_flat_library_public_modules_with_library_name() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace_by_kind(temp.path());
    write_package(temp.path(), "app", &["std"]);
    write(&temp.path().join("packages/app/core.lua"), "return {}\n");
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

[exports]
public = ["std.*"]
"#,
    );
    write(&temp.path().join("std/a.lua"), "return {}\n");
    write(&temp.path().join("std/init.lua"), "return {}\n");
    write(&temp.path().join("std/sub/b.lua"), "return {}\n");

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let scope = catalog.require_scope_for_unit("app").unwrap();
    let std_unit = catalog.units().find(|unit| unit.name() == "std").unwrap();

    assert!(scope.resolve("core").is_some());
    assert!(scope.resolve("std").is_some());
    assert!(scope.resolve("std.a").is_some());
    assert!(scope.resolve("std.sub.b").is_some());
    assert!(scope.resolve("a").is_none());
    assert!(scope.resolve("std.x").is_none());
    assert_eq!(
        std_unit
            .public_modules()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "std".to_string(),
            "std.a".to_string(),
            "std.sub.b".to_string()
        ]
    );
}

#[test]
fn catalog_accepts_exact_public_exports_matching_actual_modules() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write(
        &temp.path().join("libraries/contract/fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"

[exports]
public = ["contract.error_facts", "contract.payload", "contract.source_ref", "contract.strings"]
exact = true
"#,
    );
    write(
        &temp
            .path()
            .join("libraries/contract/public/error_facts.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/contract/public/payload.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/contract/public/source_ref.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/contract/public/strings.lua"),
        "return {}\n",
    );

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let contract = catalog
        .units()
        .find(|unit| unit.name() == "contract")
        .unwrap();

    assert_eq!(
        contract
            .public_modules()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            "contract.error_facts".to_string(),
            "contract.payload".to_string(),
            "contract.source_ref".to_string(),
            "contract.strings".to_string(),
        ]
    );
}

#[test]
fn catalog_rejects_exact_public_exports_with_extra_actual_module() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write(
        &temp.path().join("libraries/contract/fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"

[exports]
public = ["contract.error_facts"]
exact = true
"#,
    );
    write(
        &temp
            .path()
            .join("libraries/contract/public/error_facts.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/contract/public/payload.lua"),
        "return {}\n",
    );

    let err = UnitCatalog::discover(temp.path()).unwrap_err();

    assert!(err
        .to_string()
        .contains("exact exports omit public module `contract.payload`"));
}

#[test]
fn catalog_rejects_exact_public_exports_with_missing_declared_module() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write(
        &temp.path().join("libraries/contract/fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"

[exports]
public = ["contract.error_facts", "contract.payload"]
exact = true
"#,
    );
    write(
        &temp
            .path()
            .join("libraries/contract/public/error_facts.lua"),
        "return {}\n",
    );

    let err = UnitCatalog::discover(temp.path()).unwrap_err();

    assert!(err
        .to_string()
        .contains("exact exports declare missing public module `contract.payload`"));
}

#[test]
fn catalog_keeps_pattern_export_matching_when_exact_absent() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write(
        &temp.path().join("libraries/contract/fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"

[exports]
public = ["contract.*"]
"#,
    );
    write(
        &temp.path().join("libraries/contract/public/payload.lua"),
        "return {}\n",
    );
    write(
        &temp.path().join("libraries/contract/public/source_ref.lua"),
        "return {}\n",
    );

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let contract = catalog
        .units()
        .find(|unit| unit.name() == "contract")
        .unwrap();

    assert!(contract.public_modules().contains_key("contract.payload"));
    assert!(contract
        .public_modules()
        .contains_key("contract.source_ref"));
}

#[test]
fn catalog_rejects_export_pattern_that_matches_no_prefixed_public_module() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace_by_kind(temp.path());
    write_package(temp.path(), "app", &["std"]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
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

[exports]
public = ["other.*"]
"#,
    );
    write(&temp.path().join("std/a.lua"), "return {}\n");

    let err = UnitCatalog::discover(temp.path()).unwrap_err();

    assert!(err
        .to_string()
        .contains("export pattern `other.*` matches no public modules"));
}

#[test]
fn library_manifest_rejects_unknown_exports_field() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"

[exports]
public = ["contract.payload"]
unexpected = true
"#,
    );

    let err = UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("unknown field `unexpected`"), "{msg}");
}

#[test]
fn catalog_skips_legacy_std_symlink() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library(temp.path(), "std");
    write(
        &temp.path().join("libraries/std/public/fkst/json.lua"),
        "return {}\n",
    );

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            temp.path().join("libraries/std/public"),
            temp.path().join("packages/app/std"),
        )
        .unwrap();
    }

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let scope = catalog.require_scope_for_unit("app").unwrap();

    assert!(scope.resolve("main").is_some());
    assert!(scope.resolve("std.fkst.json").is_none());
    assert!(scope.resolve("fkst.json").is_none());
}

#[test]
fn catalog_namespaces_same_physical_module_names_by_library() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &["alpha", "beta"]);
    write_library(temp.path(), "alpha");
    write(&temp.path().join("libraries/alpha/public/shared.lua"), "");
    write_library(temp.path(), "beta");
    write(&temp.path().join("libraries/beta/public/shared.lua"), "");

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();
    let scope = catalog.require_scope_for_unit("app").unwrap();

    assert!(scope.resolve("alpha.shared").is_some());
    assert!(scope.resolve("beta.shared").is_some());
    assert!(scope.resolve("shared").is_none());
}

#[test]
fn library_manifest_rejects_unknown_dependency_constraints_field() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "library"
name = "contract"

[code]
root = "."

[library]
name = "contract"
stable_id = "contract"
version = "0.1.0"

[dependency_constraints]
allowed_lib_deps = []
unexpected = true
"#,
    );

    let err = UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("unknown field `unexpected`"), "{msg}");
}

#[test]
fn catalog_rejects_lib_dep_disallowed_by_visibility_allowlist() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &["restricted"]);
    write_library_allowing(temp.path(), "restricted", &["allowed-app"]);
    write(
        &temp.path().join("libraries/restricted/public/tool.lua"),
        "return {}\n",
    );

    let err = UnitCatalog::discover(temp.path()).unwrap_err();

    assert!(err
        .to_string()
        .contains("unit `app` is not allowed to declare library `restricted`"));
}

#[test]
fn catalog_rejects_library_lib_deps_outside_allowed_constraint() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library_with_dependency_constraints(
        temp.path(),
        "contract",
        &["json", "strings"],
        Some(&["json"]),
    );
    write_library(temp.path(), "json");
    write_library(temp.path(), "strings");

    let err = UnitCatalog::discover(temp.path()).unwrap_err();
    let msg = format!("{err:#}");

    assert!(
        msg.contains("lib_dep `strings` outside dependency_constraints.allowed_lib_deps"),
        "{msg}"
    );
}

#[test]
fn catalog_accepts_library_lib_deps_inside_allowed_constraint() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library_with_dependency_constraints(
        temp.path(),
        "contract",
        &["json"],
        Some(&["json", "strings"]),
    );
    write_library(temp.path(), "json");

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();

    assert_eq!(
        catalog.graph().lib_deps_for("contract"),
        Some(&["json".to_string()][..])
    );
}

#[test]
fn catalog_rejects_any_library_lib_dep_when_allowed_constraint_is_empty() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library_with_dependency_constraints(temp.path(), "contract", &["json"], Some(&[]));
    write_library(temp.path(), "json");

    let err = UnitCatalog::discover(temp.path()).unwrap_err();
    let msg = format!("{err:#}");

    assert!(
        msg.contains("lib_dep `json` outside dependency_constraints.allowed_lib_deps"),
        "{msg}"
    );
}

#[test]
fn catalog_keeps_library_lib_deps_unconstrained_when_constraint_absent() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/main.lua"), "return {}\n");
    write_library_with_dependency_constraints(temp.path(), "contract", &["json"], None);
    write_library(temp.path(), "json");

    let catalog = UnitCatalog::discover(temp.path()).unwrap().unwrap();

    assert_eq!(
        catalog.graph().lib_deps_for("contract"),
        Some(&["json".to_string()][..])
    );
}

#[test]
fn catalog_rejects_file_and_init_module_collision() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    fs::create_dir_all(temp.path().join("libraries")).unwrap();
    write_package(temp.path(), "app", &[]);
    write(&temp.path().join("packages/app/foo.lua"), "");
    write(&temp.path().join("packages/app/foo/init.lua"), "");

    let err = UnitCatalog::discover(temp.path()).unwrap_err();
    assert!(err.to_string().contains("duplicate logical module `foo`"));
}

#[test]
fn workspace_unit_pattern_must_not_escape_workspace_root() {
    let temp = tempfile::tempdir().unwrap();
    let host = temp.path().join("host");
    let external = temp.path().join("external/pkg");
    write(
        &host.join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["../external/pkg"]
"#,
    );
    write(
        &external.join("fkst.toml"),
        r#"
kind = "package"
name = "external-pkg"
persistence_class = "stateless_adapter"

[code]
root = "."
"#,
    );

    let err = UnitCatalog::discover(&host).unwrap_err();
    let msg = format!("{err:#}");

    assert!(msg.contains("must not contain `..`"), "{msg}");
}

#[test]
fn unit_code_root_must_not_escape_workspace_root() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["packages/app"]
"#,
    );
    write(
        &temp.path().join("packages/app/fkst.toml"),
        r#"
kind = "package"
name = "app"
persistence_class = "stateless_adapter"

[code]
root = "../../../outside"
"#,
    );
    fs::create_dir_all(temp.path().parent().unwrap().join("outside")).unwrap();

    let err = UnitCatalog::discover(temp.path()).unwrap_err();
    let msg = format!("{err:#}");

    assert!(
        msg.contains("code root") && msg.contains("must stay under workspace root"),
        "{msg}"
    );
}

#[test]
fn missing_workspace_manifest_returns_no_catalog() {
    let temp = tempfile::tempdir().unwrap();

    assert!(UnitCatalog::discover(temp.path()).unwrap().is_none());
}

#[test]
fn workspace_discovery_does_not_cross_nested_workspace_boundary() {
    let host = tempfile::tempdir().unwrap();
    write(
        &host.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = [
  ".fkst/local-packages/*",
  ".fkst/std",
  ".fkst/run/fkst-packages-conformance/packages/*",
]
"#,
    );
    write(
        &host
            .path()
            .join(".fkst/local-packages/site-board/fkst.toml"),
        r#"
kind = "package"
name = "site-board"
persistence_class = "stateless_adapter"

[code]
root = "."
"#,
    );
    write(
        &host.path().join(".fkst/std/fkst.toml"),
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
    let platform = host.path().join(".fkst/run/fkst-packages-conformance");
    write_workspace(&platform);
    write_package(&platform, "idle-detector", &["workflow"]);
    write_library(&platform, "workflow");

    let host_catalog = UnitCatalog::discover(host.path()).unwrap().unwrap();
    assert!(host_catalog.contains_unit("site-board"));
    assert!(host_catalog.contains_unit("std"));
    assert!(!host_catalog.contains_unit("idle-detector"));
    assert!(host_catalog.library_unit_name("workflow").is_none());

    let platform_catalog = UnitCatalog::discover(&platform).unwrap().unwrap();
    assert!(platform_catalog.contains_unit("idle-detector"));
    assert_eq!(
        platform_catalog.library_unit_name("workflow"),
        Some("workflow")
    );
}

#[test]
fn package_manifest_missing_persistence_class_parses_as_absent() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "package"
name = "app"

[code]
root = "."
"#,
    );

    let runtime_manifest = UnitManifest::parse_file(&temp.path().join("fkst.toml")).unwrap();
    let strict_manifest = UnitManifest::parse_file_strict(&temp.path().join("fkst.toml")).unwrap();

    assert_eq!(runtime_manifest.persistence_class(), None);
    assert_eq!(strict_manifest.persistence_class(), None);
}

#[test]
fn package_manifest_parses_valid_persistence_classes() {
    for (raw, class) in [
        ("saga", PersistenceClass::Saga),
        ("stateless_adapter", PersistenceClass::StatelessAdapter),
        ("judgment_pipeline", PersistenceClass::JudgmentPipeline),
        (
            "composed_judgment_pipeline",
            PersistenceClass::ComposedJudgmentPipeline,
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        write(
            &temp.path().join("fkst.toml"),
            &format!(
                r#"
kind = "package"
name = "app"
persistence_class = "{raw}"

[code]
root = "."
"#,
            ),
        );

        let manifest = UnitManifest::parse_file_strict(&temp.path().join("fkst.toml")).unwrap();

        assert_eq!(manifest.persistence_class(), Some(class));
    }
}

#[test]
fn library_manifest_rejects_persistence_class() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "library"
name = "std"
persistence_class = "stateless_adapter"

[code]
root = "."

[library]
name = "std"
stable_id = "std"
version = "0.1.0"
"#,
    );

    for parse in [UnitManifest::parse_file, UnitManifest::parse_file_strict] {
        let err = parse(&temp.path().join("fkst.toml")).unwrap_err();
        let msg = format!("{err:#}");

        assert!(
            msg.contains("library manifest must not declare `persistence_class`"),
            "{msg}"
        );
    }
}

#[test]
fn package_manifest_unknown_persistence_class_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.toml"),
        r#"
kind = "package"
name = "app"
persistence_class = "session_cache"

[code]
root = "."
"#,
    );

    for parse in [UnitManifest::parse_file, UnitManifest::parse_file_strict] {
        let err = parse(&temp.path().join("fkst.toml")).unwrap_err();
        let msg = format!("{err:#}");

        assert!(msg.contains("unknown variant `session_cache`"), "{msg}");
    }
}
