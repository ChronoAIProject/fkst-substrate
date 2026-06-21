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

#[test]
fn parses_workspace_unit_and_lock_manifests() {
    let temp = tempfile::tempdir().unwrap();
    write(
        &temp.path().join("fkst.workspace.toml"),
        r#"
[workspace]
units = ["packages/*", "libraries/*"]

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

    assert_eq!(workspace.discovered_units(), &["packages/*", "libraries/*"]);
    assert_eq!(
        workspace.registries().get("workspace").unwrap(),
        "workspace"
    );
    assert_eq!(lockfile.entries().len(), 1);
    assert_eq!(unit.kind, UnitKind::Package(PackageKind::Flat));
    assert_eq!(unit.name, "app");
    assert_eq!(unit.code_root, Path::new("."));
    assert_eq!(unit.lib_deps, vec![LibDep::new("std")]);
    assert_eq!(unit.event_deps, vec![EventDep::new("host")]);
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
    assert!(scope.resolve("fkst.json").is_some());
    assert!(scope.resolve("secret").is_none());
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
fn catalog_rejects_visible_library_module_ambiguity() {
    let temp = tempfile::tempdir().unwrap();
    write_workspace(temp.path());
    write_package(temp.path(), "app", &["alpha", "beta"]);
    write_library(temp.path(), "alpha");
    write(&temp.path().join("libraries/alpha/public/shared.lua"), "");
    write_library(temp.path(), "beta");
    write(&temp.path().join("libraries/beta/public/shared.lua"), "");

    let err = UnitCatalog::discover(temp.path()).unwrap_err();
    assert!(err.to_string().contains("ambiguous module `shared`"));
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
fn missing_workspace_manifest_is_legacy_mode() {
    let temp = tempfile::tempdir().unwrap();

    assert!(UnitCatalog::discover(temp.path()).unwrap().is_none());
}
