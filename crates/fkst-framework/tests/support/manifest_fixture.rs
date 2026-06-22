use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub fn write_single_package_workspace(root: &Path) {
    write_package_manifest(root, &unit_name(root), &[]);
    write_workspace(root, &[root]);
}

pub fn write_package_manifest(root: &Path, name: &str, lib_deps: &[&str]) {
    let deps = quoted_list(lib_deps);
    write(
        &root.join("fkst.toml"),
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

pub fn write_library_manifest(root: &Path, name: &str, lib_deps: &[&str]) {
    let deps = quoted_list(lib_deps);
    write(
        &root.join("fkst.toml"),
        &format!(
            r#"
kind = "library"
name = "{name}"

[code]
root = "."

[lib_deps]
libraries = [{deps}]

[library]
name = "{name}"
stable_id = "{name}"
version = "0.1.0"
"#
        ),
    );
}

pub fn write_workspace_for_roots(host_root: &Path, package_roots: &[&Path]) {
    write_package_manifest(host_root, "host", &[]);
    for package_root in package_roots {
        write_package_manifest(package_root, &unit_name(package_root), &[]);
    }
    let mut roots = Vec::with_capacity(package_roots.len() + 1);
    roots.push(host_root);
    roots.extend(
        package_roots
            .iter()
            .copied()
            .filter(|package_root| package_root.starts_with(host_root)),
    );
    write_workspace(host_root, &roots);
    for package_root in package_roots {
        write_workspace(package_root, &[*package_root]);
    }
}

pub fn write_workspace(host_root: &Path, unit_roots: &[&Path]) {
    let mut seen = BTreeSet::new();
    let units = unit_roots
        .iter()
        .filter_map(|root| {
            let relative = relative_path(host_root, root);
            if seen.insert(relative.clone()) {
                Some(format!(r#""{}""#, relative.display()))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    write(
        &host_root.join("fkst.workspace.toml"),
        &format!(
            r#"
[workspace]
units = [{units}]
"#
        ),
    );
}

pub fn unit_name(root: &Path) -> String {
    let raw = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unit");
    let mut name = raw
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => byte as char,
            _ => '_',
        })
        .collect::<String>();
    if name.is_empty() {
        name = "unit".to_string();
    }
    name
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
}

fn quoted_list(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!(r#""{value}""#))
        .collect::<Vec<_>>()
        .join(", ")
}

fn relative_path(base: &Path, target: &Path) -> PathBuf {
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let target = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let base_components = normal_components(&base);
    let target_components = normal_components(&target);
    let common = base_components
        .iter()
        .zip(&target_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..base_components.len() {
        relative.push("..");
    }
    for component in &target_components[common..] {
        relative.push(component);
    }
    if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    }
}

fn normal_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().into_owned()),
            Component::RootDir => Some(std::path::MAIN_SEPARATOR.to_string()),
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            Component::CurDir => None,
            Component::ParentDir => Some("..".to_string()),
        })
        .collect()
}
