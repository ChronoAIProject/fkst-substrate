use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn make_payload(root: &Path, version: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::create_dir_all(root.join("share/fkst")).unwrap();
    write_executable(&root.join("bin/fkst-supervisor"), "#!/bin/sh\nexit 0\n");
    write_executable(&root.join("bin/fkst-framework"), "#!/bin/sh\nexit 0\n");
    write_executable(&root.join("bin/fkst"), "#!/bin/sh\nexit 0\n");
    fs::write(root.join("share/fkst/VERSION"), format!("{version}\n")).unwrap();
    fs::write(root.join("install.sh"), "#!/bin/sh\nexit 0\n").unwrap();
}

fn make_fake_tools(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    write_executable(
        &dir.join("curl"),
        r#"#!/usr/bin/env bash
set -euo pipefail
output=""
url=""
while [[ "$#" -gt 0 ]]; do
  case "$1" in
    -o) output="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
[[ -n "${FKST_TEST_CURL_LOG:-}" ]] && printf '%s\n' "$url" >>"$FKST_TEST_CURL_LOG"
case "${FKST_TEST_CURL_MODE:-ok}:$url" in
  fail:*) exit 22 ;;
  ok:*"/releases/latest")
    printf '{"tag_name":"%s"}\n' "${FKST_TEST_FIRST_TAG:-v9.9.9-beta}"
    ;;
  ok:*"/releases")
    printf '[{"tag_name":"%s"},{"tag_name":"%s"}]\n' "${FKST_TEST_FIRST_TAG:-v9.9.9-beta}" "${FKST_TEST_SECOND_TAG:-v9.9.8}"
    ;;
  ok:*".tar.gz")
    [[ -n "$output" ]] || exit 64
    cp "${FKST_TEST_ARCHIVE:?}" "$output"
    ;;
  ok:*"checksums.txt")
    [[ -n "$output" ]] || exit 64
    cp "${FKST_TEST_CHECKSUMS:?}" "$output"
    ;;
  *) exit 65 ;;
esac
"#,
    );
    write_executable(
        &dir.join("tar"),
        r#"#!/usr/bin/env bash
exec /usr/bin/tar "$@"
"#,
    );
    write_executable(
        &dir.join("sha256sum"),
        r#"#!/usr/bin/env bash
shasum -a 256 "$@"
"#,
    );
}

fn make_archive(tmp: &Path, version: &str, target: &str) -> std::path::PathBuf {
    let stage = tmp.join(format!("stage-{version}"));
    let root = stage.join(format!("fkst-{version}-{target}"));
    make_payload(&root, version);
    let archive = tmp.join(format!("fkst-{version}-{target}.tar.gz"));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .arg(format!("fkst-{version}-{target}"))
        .status()
        .unwrap();
    assert!(status.success());
    archive
}

fn write_checksums(path: &Path, archive: &Path) {
    let output = Command::new("shasum")
        .arg("-a")
        .arg("256")
        .arg(archive)
        .output()
        .unwrap();
    assert!(output.status.success());
    let digest = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    fs::write(
        path,
        format!(
            "{digest}  {}\n",
            archive.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();
}

fn install_current(prefix: &Path, version: &str) {
    let root = prefix.join("lib/fkst").join(version);
    make_payload(&root, version);
    fs::remove_file(prefix.join("lib/fkst/current")).ok();
    std::os::unix::fs::symlink(version, prefix.join("lib/fkst/current")).unwrap();
}

fn install_current_without_version_file(prefix: &Path, version: &str) {
    let root = prefix.join("lib/fkst").join(version);
    make_payload(&root, version);
    fs::remove_file(root.join("share/fkst/VERSION")).unwrap();
    fs::remove_file(prefix.join("lib/fkst/current")).ok();
    std::os::unix::fs::symlink(version, prefix.join("lib/fkst/current")).unwrap();
}

fn installed_framework_bin(prefix: &Path, version: &str) -> std::path::PathBuf {
    let path = prefix
        .join("lib/fkst")
        .join(version)
        .join("bin/fkst-framework");
    fs::copy(framework_bin(), &path).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn check_only_reports_available_update() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--check-only")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("v1.0.0 -> v9.9.9-beta"));
}

#[test]
fn check_only_detects_current_from_exe_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    make_fake_tools(&tools);
    install_current_without_version_file(&prefix, "v1.2.3-beta");
    let installed_bin = installed_framework_bin(&prefix, "v1.2.3-beta");

    let output = Command::new(installed_bin)
        .arg("update")
        .arg("--check-only")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("v1.2.3-beta -> v9.9.9-beta"),
        "stdout={stdout}"
    );
    assert!(!stdout.contains("unknown"), "stdout={stdout}");
}

#[test]
fn current_release_from_exe_layout_does_not_download_or_reinstall() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    let log = tmp.path().join("curl.log");
    make_fake_tools(&tools);
    install_current_without_version_file(&prefix, "v9.9.9-beta");
    let installed_bin = installed_framework_bin(&prefix, "v9.9.9-beta");

    let output = Command::new(installed_bin)
        .arg("update")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_CURL_LOG", &log)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fkst-framework already-latest: v9.9.9-beta"),
        "stdout={stdout}"
    );
    let curl_log = fs::read_to_string(log).unwrap();
    assert!(curl_log.contains("/releases/latest"));
    assert!(!curl_log.contains(".tar.gz"), "curl_log={curl_log}");
    assert!(!curl_log.contains("checksums.txt"), "curl_log={curl_log}");
    assert_eq!(
        fs::read_link(prefix.join("lib/fkst/current")).unwrap(),
        Path::new("v9.9.9-beta")
    );
}

#[test]
fn explicit_update_ignores_start_update_check_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    let log = tmp.path().join("curl.log");
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--check-only")
        .arg("--quiet")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_UPDATE_CHECK", "0")
        .env("FKST_TEST_CURL_LOG", &log)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let curl_log = fs::read_to_string(log).unwrap();
    assert!(curl_log.contains("/releases/latest"));
}

#[test]
fn check_only_is_empty_when_current() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    make_fake_tools(&tools);
    install_current(&prefix, "v9.9.9-beta");

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--check-only")
        .arg("--quiet")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn update_downloads_verifies_and_swaps_current() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    let target = "x86_64-unknown-linux-gnu";
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");
    let archive = make_archive(tmp.path(), "v9.9.9-beta", target);
    let checksums = tmp.path().join("checksums.txt");
    write_checksums(&checksums, &archive);

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg(target)
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_ARCHIVE", &archive)
        .env("FKST_TEST_CHECKSUMS", &checksums)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(prefix.join("lib/fkst/current")).unwrap(),
        Path::new("v9.9.9-beta")
    );
    assert!(prefix
        .join("lib/fkst/v9.9.9-beta/bin/fkst-framework")
        .exists());
}

#[test]
fn explicit_update_release_query_failure_is_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_CURL_MODE", "fail")
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("release-query-failed"), "stderr={stderr}");
    assert_eq!(
        fs::read_link(prefix.join("lib/fkst/current")).unwrap(),
        Path::new("v1.0.0")
    );
}

#[test]
fn checksum_mismatch_refuses_install() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    let target = "x86_64-unknown-linux-gnu";
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");
    let archive = make_archive(tmp.path(), "v9.9.9-beta", target);
    let checksums = tmp.path().join("checksums.txt");
    fs::write(
        &checksums,
        format!(
            "{:064}  {}\n",
            0,
            archive.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg(target)
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_ARCHIVE", &archive)
        .env("FKST_TEST_CHECKSUMS", &checksums)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checksum-mismatch"), "stderr={stderr}");
    assert_eq!(
        fs::read_link(prefix.join("lib/fkst/current")).unwrap(),
        Path::new("v1.0.0")
    );
    assert!(!prefix.join("lib/fkst/v9.9.9-beta").exists());
}

#[test]
fn existing_release_directory_refuses_swap() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    let target = "x86_64-unknown-linux-gnu";
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");
    fs::create_dir_all(prefix.join("lib/fkst/v9.9.9-beta")).unwrap();
    let archive = make_archive(tmp.path(), "v9.9.9-beta", target);
    let checksums = tmp.path().join("checksums.txt");
    write_checksums(&checksums, &archive);

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg(target)
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_ARCHIVE", &archive)
        .env("FKST_TEST_CHECKSUMS", &checksums)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        fs::read_link(prefix.join("lib/fkst/current")).unwrap(),
        Path::new("v1.0.0")
    );
}

#[test]
fn network_failure_in_check_only_does_not_block() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--check-only")
        .arg("--quiet")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_CURL_MODE", "fail")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn disabled_update_exits_without_network() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let log = tmp.path().join("curl.log");
    make_fake_tools(&tools);

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--disabled")
        .arg("--prefix")
        .arg(tmp.path().join("prefix"))
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_CURL_LOG", &log)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(!log.exists());
}

#[test]
fn channel_and_repo_url_select_release_url() {
    let tmp = tempfile::tempdir().unwrap();
    let tools = tmp.path().join("bin");
    let prefix = tmp.path().join("prefix");
    let log = tmp.path().join("curl.log");
    make_fake_tools(&tools);
    install_current(&prefix, "v1.0.0");

    let output = Command::new(framework_bin())
        .arg("update")
        .arg("--check-only")
        .arg("--prefix")
        .arg(&prefix)
        .arg("--target")
        .arg("x86_64-unknown-linux-gnu")
        .arg("--channel")
        .arg("stable")
        .arg("--repo-url")
        .arg("https://github.com/ExampleOrg/fkst.git")
        .env(
            "PATH",
            format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
        )
        .env("FKST_TEST_CURL_LOG", &log)
        .env("FKST_TEST_FIRST_TAG", "v9.9.9-beta")
        .env("FKST_TEST_SECOND_TAG", "v9.9.8")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("v1.0.0 -> v9.9.8"));
    let curl_log = fs::read_to_string(log).unwrap();
    assert!(curl_log.contains("https://api.github.com/repos/ExampleOrg/fkst/releases"));
}
