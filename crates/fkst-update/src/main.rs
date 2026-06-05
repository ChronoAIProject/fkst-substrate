//! fkst-update: a thin deploy client for the external release pipeline.
//!
//! It downloads a published release artifact (`fkst-<target>.tar.gz` + `SHA256SUMS`)
//! from GitHub Releases, verifies the SHA-256 checksum, and atomically swaps the
//! installed `fkst-supervisor` and `fkst-framework` binaries in the operator bin dir.
//!
//! It is ONLY a verify+swap client. It does NOT own accepted-state, known-good,
//! rollback, health gating, canary, or process restart: those stay external policy.
//! It never signals or restarts a running supervisor; the operator restarts.
//!
//! CLI: `fkst-update [--tag <tag>] [--bin-dir <dir>] [--repo <owner/name>] [--timeout <secs>]`

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Build target triple, embedded by build.rs; names the release archive.
const TARGET: &str = env!("FKST_UPDATE_TARGET");
const DEFAULT_REPO: &str = "ChronoAIProject/fkst-substrate";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// The exact binaries the release archive must contain and this client swaps.
const BINARIES: [&str; 2] = ["fkst-supervisor", "fkst-framework"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ErrorClass {
    BadArgs,
    BinDirNotFound,
    ReleaseQueryTimeout,
    ReleaseQueryFailed,
    NoMatchingRelease,
    AssetNotFound,
    DownloadFailed,
    ChecksumMismatch,
    ExtractFailed,
    PayloadInvalid,
    SwapFailed,
}

impl ErrorClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::BadArgs => "bad-args",
            Self::BinDirNotFound => "bin-dir-not-found",
            Self::ReleaseQueryTimeout => "release-query-timeout",
            Self::ReleaseQueryFailed => "release-query-failed",
            Self::NoMatchingRelease => "no-matching-release",
            Self::AssetNotFound => "asset-not-found",
            Self::DownloadFailed => "download-failed",
            Self::ChecksumMismatch => "checksum-mismatch",
            Self::ExtractFailed => "extract-failed",
            Self::PayloadInvalid => "payload-invalid",
            Self::SwapFailed => "swap-failed",
        }
    }
}

#[derive(Debug)]
struct UpdateError {
    class: ErrorClass,
    detail: String,
}

impl UpdateError {
    fn new(class: ErrorClass, detail: impl Into<String>) -> Self {
        Self {
            class,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fkst-update {}: {}", self.class.as_str(), self.detail)
    }
}

type UpdateResult<T> = Result<T, UpdateError>;

struct Options {
    tag: Option<String>,
    bin_dir: PathBuf,
    repo: String,
    timeout: Duration,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_options(&args) {
        Ok(None) => {
            print_usage();
        }
        Ok(Some(options)) => match run(&options) {
            Ok(message) => {
                println!("{message}");
            }
            Err(err) => {
                eprintln!("{err}");
                std::process::exit(1);
            }
        },
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!(
        "usage: fkst-update [--tag <tag>] [--bin-dir <dir>] [--repo <owner/name>] [--timeout <secs>]\n\
         downloads the published release archive for {TARGET}, verifies its SHA-256\n\
         checksum, and atomically swaps fkst-supervisor and fkst-framework in the bin dir.\n\
         it does not restart a running supervisor."
    );
}

fn parse_options(args: &[String]) -> UpdateResult<Option<Options>> {
    let mut tag = None;
    let mut bin_dir: Option<PathBuf> = None;
    let mut repo = DEFAULT_REPO.to_string();
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--tag" => tag = Some(next_value(&mut iter, "--tag")?),
            "--bin-dir" => bin_dir = Some(PathBuf::from(next_value(&mut iter, "--bin-dir")?)),
            "--repo" => repo = next_value(&mut iter, "--repo")?,
            "--timeout" => {
                let secs: u64 = next_value(&mut iter, "--timeout")?.parse().map_err(|_| {
                    UpdateError::new(ErrorClass::BadArgs, "--timeout expects seconds")
                })?;
                timeout = Duration::from_secs(secs);
            }
            other => {
                return Err(UpdateError::new(
                    ErrorClass::BadArgs,
                    format!("unknown argument: {other}"),
                ))
            }
        }
    }
    let bin_dir = match bin_dir {
        Some(dir) => dir,
        None => default_bin_dir()?,
    };
    Ok(Some(Options {
        tag,
        bin_dir,
        repo,
        timeout,
    }))
}

fn next_value<'a>(iter: &mut impl Iterator<Item = &'a String>, flag: &str) -> UpdateResult<String> {
    iter.next()
        .cloned()
        .ok_or_else(|| UpdateError::new(ErrorClass::BadArgs, format!("{flag} expects a value")))
}

/// --bin-dir, else $FKST_HOME/bin, else $HOME/fkst/bin. This is where install.sh
/// placed the binaries; the updater carries no engine configuration.
fn default_bin_dir() -> UpdateResult<PathBuf> {
    if let Some(home) = std::env::var_os("FKST_HOME") {
        return Ok(PathBuf::from(home).join("bin"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join("fkst").join("bin"));
    }
    Err(UpdateError::new(
        ErrorClass::BinDirNotFound,
        "set --bin-dir, FKST_HOME, or HOME",
    ))
}

#[derive(Debug)]
struct ReleaseAsset {
    tag: String,
    archive_name: String,
    archive_url: String,
    checksums_url: String,
}

fn run(options: &Options) -> UpdateResult<String> {
    if !options.bin_dir.is_dir() {
        return Err(UpdateError::new(
            ErrorClass::BinDirNotFound,
            format!("bin dir does not exist: {}", options.bin_dir.display()),
        ));
    }
    let asset = resolve_release(options)?;

    let work = make_work_dir()?;
    let result = install_asset(options, &asset, &work);
    let _ = fs::remove_dir_all(&work);
    result?;

    Ok(format!(
        "fkst-update: swapped {} in {} to {} (restart the supervisor to run it)",
        BINARIES.join(", "),
        options.bin_dir.display(),
        asset.tag
    ))
}

fn install_asset(options: &Options, asset: &ReleaseAsset, work: &Path) -> UpdateResult<()> {
    let archive = work.join(&asset.archive_name);
    let checksums = work.join("SHA256SUMS");
    download_to(&asset.archive_url, &archive, options.timeout)?;
    download_to(&asset.checksums_url, &checksums, options.timeout)?;
    verify_checksum(&archive, &checksums, &asset.archive_name, options.timeout)?;

    let extract = work.join("extract");
    fs::create_dir_all(&extract)
        .map_err(|err| UpdateError::new(ErrorClass::ExtractFailed, err.to_string()))?;
    extract_archive(&archive, &extract, options.timeout)?;
    validate_payload(&extract)?;
    swap_binaries(&extract, &options.bin_dir)
}

fn resolve_release(options: &Options) -> UpdateResult<ReleaseAsset> {
    let api_url = match &options.tag {
        Some(tag) => format!(
            "https://api.github.com/repos/{}/releases/tags/{tag}",
            options.repo
        ),
        None => format!(
            "https://api.github.com/repos/{}/releases/latest",
            options.repo
        ),
    };
    let outcome = run_command("curl", &["-fsSL", &api_url], options.timeout)
        .map_err(|err| UpdateError::new(ErrorClass::ReleaseQueryFailed, err.to_string()))?;
    if outcome.timed_out {
        return Err(UpdateError::new(ErrorClass::ReleaseQueryTimeout, api_url));
    }
    if outcome.status != Some(0) {
        return Err(UpdateError::new(
            ErrorClass::ReleaseQueryFailed,
            String::from_utf8_lossy(&outcome.stderr).trim().to_string(),
        ));
    }
    let release: serde_json::Value = serde_json::from_slice(&outcome.stdout)
        .map_err(|err| UpdateError::new(ErrorClass::ReleaseQueryFailed, err.to_string()))?;
    release_asset_from_json(&release)
}

/// Pure asset selection from a GitHub release JSON object (unit-testable).
fn release_asset_from_json(release: &serde_json::Value) -> UpdateResult<ReleaseAsset> {
    let tag = release
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| UpdateError::new(ErrorClass::NoMatchingRelease, "release has no tag_name"))?
        .to_string();
    let archive_name = format!("fkst-{TARGET}.tar.gz");
    let assets = release
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| UpdateError::new(ErrorClass::AssetNotFound, "release has no assets"))?;
    let url_for = |want: &str| -> Option<String> {
        assets.iter().find_map(|asset| {
            let name = asset.get("name").and_then(serde_json::Value::as_str)?;
            let url = asset
                .get("browser_download_url")
                .and_then(serde_json::Value::as_str)?;
            (name == want).then(|| url.to_string())
        })
    };
    let archive_url = url_for(&archive_name).ok_or_else(|| {
        UpdateError::new(
            ErrorClass::AssetNotFound,
            format!("release {tag} has no asset {archive_name}"),
        )
    })?;
    let checksums_url = url_for("SHA256SUMS").ok_or_else(|| {
        UpdateError::new(
            ErrorClass::AssetNotFound,
            format!("release {tag} has no asset SHA256SUMS"),
        )
    })?;
    Ok(ReleaseAsset {
        tag,
        archive_name,
        archive_url,
        checksums_url,
    })
}

fn make_work_dir() -> UpdateResult<PathBuf> {
    let dir = std::env::temp_dir().join(format!("fkst-update-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)
        .map_err(|err| UpdateError::new(ErrorClass::DownloadFailed, err.to_string()))?;
    Ok(dir)
}

fn download_to(url: &str, path: &Path, timeout: Duration) -> UpdateResult<()> {
    let out = path.to_string_lossy().to_string();
    let outcome = run_command("curl", &["-fsSL", url, "-o", &out], timeout)
        .map_err(|err| UpdateError::new(ErrorClass::DownloadFailed, err.to_string()))?;
    if outcome.timed_out {
        return Err(UpdateError::new(
            ErrorClass::DownloadFailed,
            format!("timeout: {url}"),
        ));
    }
    if outcome.status != Some(0) {
        return Err(UpdateError::new(
            ErrorClass::DownloadFailed,
            String::from_utf8_lossy(&outcome.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

fn verify_checksum(
    archive: &Path,
    checksums: &Path,
    archive_name: &str,
    timeout: Duration,
) -> UpdateResult<()> {
    let text = fs::read_to_string(checksums)
        .map_err(|err| UpdateError::new(ErrorClass::ChecksumMismatch, err.to_string()))?;
    let expected = expected_digest(&text, archive_name).ok_or_else(|| {
        UpdateError::new(
            ErrorClass::ChecksumMismatch,
            format!("no checksum entry for {archive_name}"),
        )
    })?;
    let actual = sha256_file(archive, timeout)?;
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        Err(UpdateError::new(
            ErrorClass::ChecksumMismatch,
            format!("checksum mismatch for {archive_name}"),
        ))
    }
}

/// Parse a SHA256SUMS body for `<digest>  <name>` (pure, unit-testable). The name
/// in the file may carry a leading `*` (binary mode) or path; match on basename.
fn expected_digest(text: &str, archive_name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        let base = Path::new(name).file_name()?.to_str()?;
        (base == archive_name).then(|| digest.to_string())
    })
}

fn sha256_file(path: &Path, timeout: Duration) -> UpdateResult<String> {
    let arg = path.to_string_lossy().to_string();
    if let Ok(outcome) = run_command("sha256sum", &[&arg], timeout) {
        if outcome.status == Some(0) && !outcome.timed_out {
            return digest_token(&outcome.stdout);
        }
    }
    let outcome = run_command("shasum", &["-a", "256", &arg], timeout)
        .map_err(|err| UpdateError::new(ErrorClass::ChecksumMismatch, err.to_string()))?;
    if outcome.status == Some(0) && !outcome.timed_out {
        return digest_token(&outcome.stdout);
    }
    Err(UpdateError::new(
        ErrorClass::ChecksumMismatch,
        "sha256sum and shasum both failed",
    ))
}

fn digest_token(stdout: &[u8]) -> UpdateResult<String> {
    String::from_utf8_lossy(stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| UpdateError::new(ErrorClass::ChecksumMismatch, "empty digest output"))
}

fn extract_archive(archive: &Path, dest: &Path, timeout: Duration) -> UpdateResult<()> {
    let archive_arg = archive.to_string_lossy().to_string();
    let dest_arg = dest.to_string_lossy().to_string();
    let outcome = run_command("tar", &["-xzf", &archive_arg, "-C", &dest_arg], timeout)
        .map_err(|err| UpdateError::new(ErrorClass::ExtractFailed, err.to_string()))?;
    if outcome.timed_out || outcome.status != Some(0) {
        return Err(UpdateError::new(
            ErrorClass::ExtractFailed,
            String::from_utf8_lossy(&outcome.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

/// The extracted payload must contain the expected binaries as files (extra
/// files are ignored; only these two are swapped).
fn validate_payload(extract: &Path) -> UpdateResult<()> {
    for name in BINARIES {
        let path = extract.join(name);
        if !path.is_file() {
            return Err(UpdateError::new(
                ErrorClass::PayloadInvalid,
                format!("payload missing {name}"),
            ));
        }
    }
    Ok(())
}

/// Stage ALL verified binaries first, then rename each over its target. Staging
/// before any rename means a copy failure cannot leave a half-applied swap; the
/// only window is between the consecutive renames, each atomic on the bin dir fs.
fn swap_binaries(extract: &Path, bin_dir: &Path) -> UpdateResult<()> {
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    let cleanup = |staged: &[(PathBuf, PathBuf)]| {
        for (staging, _) in staged {
            let _ = fs::remove_file(staging);
        }
    };
    for name in BINARIES {
        let src = extract.join(name);
        let dst = bin_dir.join(name);
        let staging = bin_dir.join(format!(".fkst-update-{}-{name}", std::process::id()));
        let _ = fs::remove_file(&staging);
        let staged_ok = fs::copy(&src, &staging)
            .map_err(|err| UpdateError::new(ErrorClass::SwapFailed, err.to_string()))
            .and_then(|_| set_executable(&staging));
        if let Err(err) = staged_ok {
            let _ = fs::remove_file(&staging);
            cleanup(&staged);
            return Err(err);
        }
        staged.push((staging, dst));
    }
    for (staging, dst) in &staged {
        if let Err(err) = fs::rename(staging, dst) {
            cleanup(&staged);
            return Err(UpdateError::new(ErrorClass::SwapFailed, err.to_string()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> UpdateResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|err| UpdateError::new(ErrorClass::SwapFailed, err.to_string()))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> UpdateResult<()> {
    Ok(())
}

struct CommandOutcome {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

fn run_command(program: &str, args: &[&str], timeout: Duration) -> std::io::Result<CommandOutcome> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            return Ok(CommandOutcome {
                status: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                timed_out: false,
            });
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            return Ok(CommandOutcome {
                status: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                timed_out: true,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timeout() -> Duration {
        Duration::from_secs(30)
    }

    #[test]
    fn target_triple_is_embedded() {
        assert!(!TARGET.is_empty(), "build.rs must embed FKST_UPDATE_TARGET");
    }

    #[test]
    fn expected_digest_matches_by_basename_and_star() {
        let body = "abc123  fkst-aarch64-apple-darwin.tar.gz\ndef456 *some/path/SHA256SUMS\n";
        assert_eq!(
            expected_digest(body, "fkst-aarch64-apple-darwin.tar.gz"),
            Some("abc123".to_string())
        );
        assert_eq!(
            expected_digest(body, "SHA256SUMS"),
            Some("def456".to_string())
        );
        assert_eq!(expected_digest(body, "missing.tar.gz"), None);
    }

    #[test]
    fn release_asset_selection_from_json() {
        let archive = format!("fkst-{TARGET}.tar.gz");
        let release = serde_json::json!({
            "tag_name": "v1.2.3",
            "assets": [
                {"name": archive, "browser_download_url": "https://example/archive"},
                {"name": "SHA256SUMS", "browser_download_url": "https://example/sums"},
                {"name": "fkst-other-target.tar.gz", "browser_download_url": "https://example/other"}
            ]
        });
        let asset = release_asset_from_json(&release).unwrap();
        assert_eq!(asset.tag, "v1.2.3");
        assert_eq!(asset.archive_name, archive);
        assert_eq!(asset.archive_url, "https://example/archive");
        assert_eq!(asset.checksums_url, "https://example/sums");
    }

    #[test]
    fn release_asset_missing_archive_is_asset_not_found() {
        let release = serde_json::json!({
            "tag_name": "v1.0.0",
            "assets": [{"name": "SHA256SUMS", "browser_download_url": "https://example/sums"}]
        });
        let err = release_asset_from_json(&release).unwrap_err();
        assert_eq!(err.class, ErrorClass::AssetNotFound);
    }

    #[test]
    fn verify_checksum_roundtrip_and_mismatch() {
        let dir = std::env::temp_dir().join(format!("fkst-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("fkst-test.tar.gz");
        fs::write(&archive, b"payload-bytes").unwrap();
        let digest = sha256_file(&archive, timeout()).unwrap();

        let good = dir.join("good.sums");
        fs::write(&good, format!("{digest}  fkst-test.tar.gz\n")).unwrap();
        verify_checksum(&archive, &good, "fkst-test.tar.gz", timeout()).unwrap();

        let bad = dir.join("bad.sums");
        fs::write(&bad, "0000  fkst-test.tar.gz\n").unwrap();
        let err = verify_checksum(&archive, &bad, "fkst-test.tar.gz", timeout()).unwrap_err();
        assert_eq!(err.class, ErrorClass::ChecksumMismatch);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_payload_requires_both_binaries() {
        let dir = std::env::temp_dir().join(format!("fkst-update-payload-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fkst-supervisor"), b"x").unwrap();
        let err = validate_payload(&dir).unwrap_err();
        assert_eq!(err.class, ErrorClass::PayloadInvalid);
        fs::write(dir.join("fkst-framework"), b"y").unwrap();
        validate_payload(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn swap_binaries_replaces_targets_atomically() {
        let dir = std::env::temp_dir().join(format!("fkst-update-swap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let extract = dir.join("extract");
        let bin = dir.join("bin");
        fs::create_dir_all(&extract).unwrap();
        fs::create_dir_all(&bin).unwrap();
        for name in BINARIES {
            fs::write(extract.join(name), format!("new-{name}")).unwrap();
            fs::write(bin.join(name), b"old").unwrap();
        }
        swap_binaries(&extract, &bin).unwrap();
        for name in BINARIES {
            let got = fs::read_to_string(bin.join(name)).unwrap();
            assert_eq!(got, format!("new-{name}"));
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
