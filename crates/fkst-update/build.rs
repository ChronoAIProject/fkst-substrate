// Embed the build target triple so the updater requests the matching release
// archive (fkst-<target>.tar.gz). cargo sets TARGET for build scripts.
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=FKST_UPDATE_TARGET={target}");
}
