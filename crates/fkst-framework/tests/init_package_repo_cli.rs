use std::path::Path;
use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
}

fn run_init(cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(framework_bin());
    cmd.arg("init-package-repo").current_dir(cwd);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output().unwrap()
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

fn init_git_like_repo() -> tempfile::TempDir {
    let repo = tempfile::Builder::new()
        .prefix("fkst-package-repo")
        .tempdir()
        .unwrap();
    std::fs::create_dir(repo.path().join(".git")).unwrap();
    repo
}

#[test]
fn init_package_repo_materializes_empty_repo() {
    let repo = init_git_like_repo();

    let output = run_init(repo.path(), &["--ref", "test-ref"]);

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("CREATE scripts/run.sh"), "{out}");
    assert!(out.contains("CREATE scripts/check_repo.py"), "{out}");
    assert!(out.contains("CREATE .github/workflows/ci.yml"), "{out}");
    assert!(out.contains("CREATE env.example"), "{out}");
    assert!(out.contains("CREATE .fkst-substrate-ref"), "{out}");
    assert!(out.contains("CREATE README.md"), "{out}");
    assert!(out.contains("CREATE .gitignore"), "{out}");

    assert!(repo.path().join("scripts/run.sh").is_file());
    assert!(repo.path().join("scripts/check_repo.py").is_file());
    assert!(repo.path().join(".github/workflows/ci.yml").is_file());
    assert_eq!(
        std::fs::read_to_string(repo.path().join(".fkst-substrate-ref")).unwrap(),
        "test-ref\n"
    );
    assert!(std::fs::read_to_string(repo.path().join(".gitignore"))
        .unwrap()
        .contains("fkst.env\n"));
    assert!(std::fs::read_to_string(repo.path().join("scripts/run.sh"))
        .unwrap()
        .contains("substrate_ref=\"$(< \"$repo/.fkst-substrate-ref\")\""));
    assert!(std::fs::read_to_string(repo.path().join("scripts/run.sh"))
        .unwrap()
        .contains(
            "cargo build --manifest-path \"$substrate_src/Cargo.toml\" -p fkst-framework --release"
        ));
    assert!(std::fs::read_to_string(repo.path().join("scripts/run.sh"))
        .unwrap()
        .contains("\"$framework_bin\" test --project-root \"$repo\" --package-root \"$repo\""));
}

#[test]
fn init_package_repo_converged_repo_is_noop() {
    let repo = init_git_like_repo();
    assert_exit(&run_init(repo.path(), &["--ref", "test-ref"]), 0);

    let output = run_init(repo.path(), &["--ref", "test-ref"]);

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("UNCHANGED scripts/run.sh"), "{out}");
    assert!(out.contains("UNCHANGED scripts/check_repo.py"), "{out}");
    assert!(out.contains("UNCHANGED .github/workflows/ci.yml"), "{out}");
    assert!(out.contains("UNCHANGED env.example"), "{out}");
    assert!(out.contains("UNCHANGED .fkst-substrate-ref"), "{out}");
    assert!(out.contains("UNCHANGED README.md"), "{out}");
    assert!(out.contains("UNCHANGED .gitignore"), "{out}");
}

#[test]
fn init_package_repo_refuses_local_edits_without_force() {
    let repo = init_git_like_repo();
    assert_exit(&run_init(repo.path(), &["--ref", "test-ref"]), 0);
    std::fs::write(
        repo.path().join("scripts/run.sh"),
        "#!/usr/bin/env bash\nexit 99\n",
    )
    .unwrap();

    let output = run_init(repo.path(), &["--ref", "test-ref"]);

    assert_exit(&output, 2);
    let out = stdout(&output);
    let err = stderr(&output);
    assert!(out.contains("REFUSE scripts/run.sh differs"), "{out}");
    assert!(
        out.contains("hint=compare with a fresh init-package-repo scaffold"),
        "{out}"
    );
    assert!(
        err.contains("refused local edits in scripts/run.sh"),
        "{err}"
    );
    assert!(err.contains("--force"), "{err}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("scripts/run.sh")).unwrap(),
        "#!/usr/bin/env bash\nexit 99\n"
    );
}

#[test]
fn init_package_repo_force_overwrites_local_edits() {
    let repo = init_git_like_repo();
    assert_exit(&run_init(repo.path(), &["--ref", "test-ref"]), 0);
    std::fs::write(
        repo.path().join("scripts/run.sh"),
        "#!/usr/bin/env bash\nexit 99\n",
    )
    .unwrap();

    let output = run_init(repo.path(), &["--ref", "test-ref", "--force"]);

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("OVERWRITE scripts/run.sh differs"), "{out}");
    assert!(std::fs::read_to_string(repo.path().join("scripts/run.sh"))
        .unwrap()
        .contains(
            "\"$framework_bin\" conformance --project-root \"$repo\" --package-root \"$repo\""
        ));
}

#[test]
fn init_package_repo_appends_missing_gitignore_entries() {
    let repo = init_git_like_repo();
    std::fs::write(repo.path().join(".gitignore"), "local.log\n").unwrap();

    let output = run_init(repo.path(), &["--ref", "test-ref"]);

    assert_exit(&output, 0);
    let out = stdout(&output);
    assert!(out.contains("APPEND .gitignore entries="), "{out}");
    let gitignore = std::fs::read_to_string(repo.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("local.log\n"));
    assert!(gitignore.contains(".fkst/\n"));
    assert!(gitignore.contains("fkst.env\n"));
}
