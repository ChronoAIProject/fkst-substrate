use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-env-changed=FKST_FRAMEWORK_SOURCE_PIN");

    if let Ok(pin) = std::env::var("FKST_FRAMEWORK_SOURCE_PIN") {
        println!("cargo:rustc-env=FKST_FRAMEWORK_SOURCE_PIN={pin}");
        return;
    }

    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let repo_root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap_or(&manifest_dir);
    let pin = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
        .filter(|pin| !pin.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=FKST_FRAMEWORK_SOURCE_PIN={pin}");
}
