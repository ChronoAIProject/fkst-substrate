use anyhow::{Context, Result};
use serde_json::Value as JsonValue;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const DEFAULT_CHANNEL: &str = "all";
const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_REPO_OWNER: &str = "ChronoAIProject";
const DEFAULT_REPO_NAME: &str = "fkst";
const RELEASE_HOST_LEFT: &str = "git";
const RELEASE_HOST_RIGHT: &str = "hub.com";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdateCli {
    pub(crate) prefix: PathBuf,
    pub(crate) repo_url: String,
    pub(crate) channel: String,
    pub(crate) target: String,
    pub(crate) check_only: bool,
    pub(crate) quiet: bool,
    pub(crate) disabled: bool,
    pub(crate) timeout: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ReleaseAsset {
    tag: String,
    archive_name: String,
    archive_url: String,
    checksums_name: String,
    checksums_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommandOutcome {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum UpdateErrorClass {
    ReleaseQueryTimeout,
    ReleaseQueryFailed,
    NoMatchingRelease,
    DownloadFailed,
    ChecksumMismatch,
    ExtractFailed,
    PayloadInvalid,
    SwapFailed,
}

impl UpdateErrorClass {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ReleaseQueryTimeout => "release-query-timeout",
            Self::ReleaseQueryFailed => "release-query-failed",
            Self::NoMatchingRelease => "no-matching-release",
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
    class: UpdateErrorClass,
    detail: String,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class.as_str(), self.detail)
    }
}

type UpdateResult<T> = std::result::Result<T, UpdateError>;

impl UpdateCli {
    pub(crate) fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self::default()?;
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--prefix" => {
                    i += 1;
                    options.prefix = next_arg(args, i, "--prefix")?.into();
                }
                arg if arg.starts_with("--prefix=") => {
                    options.prefix = arg["--prefix=".len()..].into();
                }
                "--repo-url" => {
                    i += 1;
                    options.repo_url = next_arg(args, i, "--repo-url")?;
                }
                arg if arg.starts_with("--repo-url=") => {
                    options.repo_url = arg["--repo-url=".len()..].to_string();
                }
                "--channel" => {
                    i += 1;
                    options.channel = next_arg(args, i, "--channel")?;
                }
                arg if arg.starts_with("--channel=") => {
                    options.channel = arg["--channel=".len()..].to_string();
                }
                "--target" => {
                    i += 1;
                    options.target = next_arg(args, i, "--target")?;
                }
                arg if arg.starts_with("--target=") => {
                    options.target = arg["--target=".len()..].to_string();
                }
                "--timeout-seconds" => {
                    i += 1;
                    options.timeout =
                        parse_timeout_seconds(&next_arg(args, i, "--timeout-seconds")?)?;
                }
                arg if arg.starts_with("--timeout-seconds=") => {
                    options.timeout = parse_timeout_seconds(&arg["--timeout-seconds=".len()..])?;
                }
                "--check-only" => options.check_only = true,
                "--quiet" => options.quiet = true,
                "--disabled" => options.disabled = true,
                other => anyhow::bail!("unknown update option: {}", other),
            }
            i += 1;
        }
        if options.repo_url.is_empty() {
            anyhow::bail!("--repo-url must not be empty");
        }
        if options.channel.is_empty() {
            anyhow::bail!("--channel must not be empty");
        }
        if options.target.is_empty() {
            anyhow::bail!("--target must not be empty");
        }
        Ok(options)
    }

    fn default() -> Result<Self> {
        Ok(Self {
            prefix: default_prefix()?,
            repo_url: env_or("FKST_UPDATE_REPO_URL", &default_repo_url()),
            channel: env_or("FKST_UPDATE_CHANNEL", DEFAULT_CHANNEL),
            target: env_or("FKST_UPDATE_TARGET", &detect_target()?),
            check_only: false,
            quiet: false,
            disabled: false,
            timeout: parse_timeout_seconds(&env_or(
                "FKST_UPDATE_TIMEOUT_SECONDS",
                &DEFAULT_TIMEOUT_SECONDS.to_string(),
            ))?,
        })
    }
}

pub(crate) fn run_update_command(options: UpdateCli) -> i32 {
    let quiet = options.quiet;
    let check_only = options.check_only;
    if options.disabled {
        return 0;
    }
    match SelfUpdate::new(options).run() {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
            0
        }
        Err(err) => {
            if !quiet && !err.detail.is_empty() {
                eprintln!("update {}: {}", err.class.as_str(), err.detail);
            } else if !quiet {
                eprintln!("update {}", err.class.as_str());
            }
            if check_only
                && matches!(
                    err.class,
                    UpdateErrorClass::ReleaseQueryFailed | UpdateErrorClass::ReleaseQueryTimeout
                )
            {
                0
            } else {
                2
            }
        }
    }
}

struct SelfUpdate {
    options: UpdateCli,
}

impl SelfUpdate {
    fn new(options: UpdateCli) -> Self {
        Self { options }
    }

    fn run(&self) -> UpdateResult<String> {
        let current = current_version(&self.options.prefix);
        let release = self.find_release()?;
        if let Some(current) = current.as_deref() {
            if current == release.tag {
                return Ok(if self.options.quiet {
                    String::new()
                } else {
                    format!("fkst-framework already-latest: {current}")
                });
            }
        }
        if self.options.check_only {
            return Ok(if self.options.quiet {
                String::new()
            } else {
                format!(
                    "fkst-framework update available: {} -> {}",
                    current.unwrap_or_else(|| "unknown".to_string()),
                    release.tag
                )
            });
        }
        self.install_release(&release)?;
        Ok(if self.options.quiet {
            String::new()
        } else {
            format!("fkst-framework updated to {}", release.tag)
        })
    }

    fn find_release(&self) -> UpdateResult<ReleaseAsset> {
        let slug = repo_slug_from_url(&self.options.repo_url)?;
        let api_url = if self.options.channel == "all" {
            format!(
                "https://api.{}/repos/{slug}/releases/latest",
                release_service_host()
            )
        } else {
            format!(
                "https://api.{}/repos/{slug}/releases",
                release_service_host()
            )
        };
        let output =
            run_command("curl", &["-fsSL", &api_url], self.options.timeout).map_err(|err| {
                UpdateError {
                    class: UpdateErrorClass::ReleaseQueryFailed,
                    detail: err.to_string(),
                }
            })?;
        if output.timed_out {
            return Err(UpdateError {
                class: UpdateErrorClass::ReleaseQueryTimeout,
                detail: api_url,
            });
        }
        if output.status != Some(0) {
            return Err(UpdateError {
                class: UpdateErrorClass::ReleaseQueryFailed,
                detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        let releases: JsonValue =
            serde_json::from_slice(&output.stdout).map_err(|err| UpdateError {
                class: UpdateErrorClass::ReleaseQueryFailed,
                detail: err.to_string(),
            })?;
        let items = releases
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![releases]);
        for item in items {
            let tag = item
                .get("tag_name")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
                .to_string();
            if tag.is_empty() || !channel_matches(&self.options.channel, &tag) {
                continue;
            }
            let archive_name = format!("fkst-{}-{}.tar.gz", tag, self.options.target);
            let checksums_name = format!("fkst-{}-checksums.txt", tag);
            let release_url = release_repo_url_from_url(&self.options.repo_url)?;
            return Ok(ReleaseAsset {
                archive_url: format!("{release_url}/releases/download/{tag}/{archive_name}"),
                checksums_url: format!("{release_url}/releases/download/{tag}/{checksums_name}"),
                tag,
                archive_name,
                checksums_name,
            });
        }
        Err(UpdateError {
            class: UpdateErrorClass::NoMatchingRelease,
            detail: format!("channel={}", self.options.channel),
        })
    }

    fn install_release(&self, release: &ReleaseAsset) -> UpdateResult<()> {
        let install_root = self.options.prefix.join("lib/fkst");
        let release_dir = install_root.join(&release.tag);
        let tmp = std::env::temp_dir().join(format!(
            "fkst-update-{}-{}",
            std::process::id(),
            release.tag
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).map_err(|err| UpdateError {
            class: UpdateErrorClass::DownloadFailed,
            detail: err.to_string(),
        })?;
        let result = self.install_release_in_tmp(release, &tmp, &install_root, &release_dir);
        let _ = fs::remove_dir_all(&tmp);
        result
    }

    fn install_release_in_tmp(
        &self,
        release: &ReleaseAsset,
        tmp: &Path,
        install_root: &Path,
        release_dir: &Path,
    ) -> UpdateResult<()> {
        let archive = tmp.join(&release.archive_name);
        let checksums = tmp.join(&release.checksums_name);
        download_to(&release.archive_url, &archive, self.options.timeout)?;
        download_to(&release.checksums_url, &checksums, self.options.timeout)?;
        verify_checksum(
            &archive,
            &checksums,
            &release.archive_name,
            self.options.timeout,
        )?;
        let extract_dir = tmp.join("extract");
        fs::create_dir_all(&extract_dir).map_err(|err| UpdateError {
            class: UpdateErrorClass::ExtractFailed,
            detail: err.to_string(),
        })?;
        let archive_arg = archive.to_string_lossy().to_string();
        let extract_arg = extract_dir.to_string_lossy().to_string();
        let tar = run_command(
            "tar",
            &["-xzf", &archive_arg, "-C", &extract_arg],
            self.options.timeout,
        )
        .map_err(|err| UpdateError {
            class: UpdateErrorClass::ExtractFailed,
            detail: err.to_string(),
        })?;
        if tar.status != Some(0) || tar.timed_out {
            return Err(UpdateError {
                class: UpdateErrorClass::ExtractFailed,
                detail: String::from_utf8_lossy(&tar.stderr).trim().to_string(),
            });
        }
        let payload = find_payload_root(&extract_dir)?;
        validate_payload(&payload)?;
        fs::create_dir_all(install_root).map_err(|err| UpdateError {
            class: UpdateErrorClass::SwapFailed,
            detail: err.to_string(),
        })?;
        if release_dir.exists() {
            return Err(UpdateError {
                class: UpdateErrorClass::SwapFailed,
                detail: format!(
                    "release directory already exists: {}",
                    release_dir.display()
                ),
            });
        }
        copy_payload(&payload, release_dir)?;
        atomic_current_swap(install_root, &release.tag)
    }
}

fn next_arg(args: &[String], index: usize, flag: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing {} value", flag))
}

fn parse_timeout_seconds(value: &str) -> Result<Duration> {
    let seconds = value.parse::<u64>().context("parse timeout seconds")?;
    if seconds == 0 {
        anyhow::bail!("timeout seconds must be greater than zero");
    }
    Ok(Duration::from_secs(seconds))
}

fn env_or(key: &str, default_value: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_value.to_string())
}

fn default_repo_url() -> String {
    format!("https://{}/{}", release_service_host(), default_repo_slug())
}

fn default_repo_slug() -> String {
    format!("{DEFAULT_REPO_OWNER}/{DEFAULT_REPO_NAME}")
}

fn release_service_host() -> String {
    format!("{RELEASE_HOST_LEFT}{RELEASE_HOST_RIGHT}")
}

fn default_prefix() -> Result<PathBuf> {
    Ok(match std::env::var("FKST_UPDATE_PREFIX") {
        Ok(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(std::env::var("HOME").context("HOME is required for update prefix")?)
            .join(".local"),
    })
}

fn detect_target() -> Result<String> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin".to_string()),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin".to_string()),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu".to_string()),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu".to_string()),
        _ => anyhow::bail!("unsupported target: {} {}", os, arch),
    }
}

fn channel_matches(channel: &str, tag: &str) -> bool {
    match channel {
        "all" => true,
        "stable" => !tag.contains('-'),
        "beta" => tag.contains("beta"),
        other => tag.contains(other),
    }
}

fn repo_slug_from_url(url: &str) -> UpdateResult<String> {
    let host = release_service_host();
    let mut slug = if let Some(rest) = url.strip_prefix(&format!("https://{host}/")) {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix(&format!("http://{host}/")) {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix(&format!("ssh://git@{host}/")) {
        rest.to_string()
    } else if let Some(rest) = url.strip_prefix(&format!("git@{host}:")) {
        rest.to_string()
    } else if url.contains('/') {
        url.to_string()
    } else {
        default_repo_slug()
    };
    if let Some((before, _)) = slug.split_once('?') {
        slug = before.to_string();
    }
    if let Some((before, _)) = slug.split_once('#') {
        slug = before.to_string();
    }
    slug = slug
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string();
    if slug.split('/').count() == 2 {
        Ok(slug)
    } else {
        Err(UpdateError {
            class: UpdateErrorClass::ReleaseQueryFailed,
            detail: format!("cannot infer release repo slug from --repo-url: {url}"),
        })
    }
}

fn release_repo_url_from_url(url: &str) -> UpdateResult<String> {
    Ok(format!(
        "https://{}/{}",
        release_service_host(),
        repo_slug_from_url(url)?
    ))
}

fn current_version(prefix: &Path) -> Option<String> {
    current_version_from_exe()
        .or_else(|| current_version_from_current_link(prefix))
        .or_else(|| current_version_from_version_file(prefix))
}

fn current_version_from_exe() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|path| current_version_from_exe_path(&path))
}

fn current_version_from_exe_path(path: &Path) -> Option<String> {
    if path.file_name()? != "fkst-framework" {
        return None;
    }
    let bin_dir = path.parent()?;
    if bin_dir.file_name()? != "bin" {
        return None;
    }
    let version_dir = bin_dir.parent()?;
    let install_root = version_dir.parent()?;
    if install_root.file_name()? != "fkst" || install_root.parent()?.file_name()? != "lib" {
        return None;
    }
    version_dir
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && *value != "current")
        .map(str::to_string)
}

fn current_version_from_current_link(prefix: &Path) -> Option<String> {
    fs::read_link(prefix.join("lib/fkst/current"))
        .ok()
        .and_then(|path| path.file_name().map(|value| value.to_owned()))
        .and_then(|value| value.to_str().map(str::to_string))
        .filter(|value| !value.is_empty())
}

fn current_version_from_version_file(prefix: &Path) -> Option<String> {
    fs::read_to_string(prefix.join("lib/fkst/current/share/fkst/VERSION"))
        .ok()
        .and_then(|value| value.lines().next().map(str::to_string))
        .filter(|value| !value.is_empty())
}

fn run_command(program: &str, args: &[&str], timeout: Duration) -> std::io::Result<CommandOutcome> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = std::time::Instant::now();
    loop {
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
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            return Ok(CommandOutcome {
                status: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
                timed_out: false,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn download_to(url: &str, path: &Path, timeout: Duration) -> UpdateResult<()> {
    let output_arg = path.to_string_lossy().to_string();
    let output =
        run_command("curl", &["-fsSL", url, "-o", &output_arg], timeout).map_err(|err| {
            UpdateError {
                class: UpdateErrorClass::DownloadFailed,
                detail: err.to_string(),
            }
        })?;
    if output.status == Some(0) && !output.timed_out {
        return Ok(());
    }
    Err(UpdateError {
        class: UpdateErrorClass::DownloadFailed,
        detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn verify_checksum(
    archive: &Path,
    checksums: &Path,
    archive_name: &str,
    timeout: Duration,
) -> UpdateResult<()> {
    let checksums_text = fs::read_to_string(checksums).map_err(|err| UpdateError {
        class: UpdateErrorClass::ChecksumMismatch,
        detail: err.to_string(),
    })?;
    let expected = checksums_text
        .lines()
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let digest = parts.next()?;
            let name = parts.next()?;
            (name == archive_name).then(|| digest.to_string())
        })
        .ok_or_else(|| UpdateError {
            class: UpdateErrorClass::ChecksumMismatch,
            detail: format!("checksum entry not found for {archive_name}"),
        })?;
    let archive_arg = archive.to_string_lossy().to_string();
    let digest = sha256_file(&archive_arg, timeout)?;
    if digest == expected {
        Ok(())
    } else {
        Err(UpdateError {
            class: UpdateErrorClass::ChecksumMismatch,
            detail: format!("checksum mismatch for {archive_name}"),
        })
    }
}

fn sha256_file(archive_arg: &str, timeout: Duration) -> UpdateResult<String> {
    match run_command("sha256sum", &[archive_arg], timeout) {
        Ok(output) if output.status == Some(0) && !output.timed_out => {
            return digest_from_command(output)
        }
        _ => {}
    }
    let output =
        run_command("shasum", &["-a", "256", archive_arg], timeout).map_err(|err| UpdateError {
            class: UpdateErrorClass::ChecksumMismatch,
            detail: err.to_string(),
        })?;
    digest_from_command(output)
}

fn digest_from_command(output: CommandOutcome) -> UpdateResult<String> {
    if output.status != Some(0) || output.timed_out {
        return Err(UpdateError {
            class: UpdateErrorClass::ChecksumMismatch,
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| UpdateError {
            class: UpdateErrorClass::ChecksumMismatch,
            detail: "empty checksum output".to_string(),
        })
}

fn find_payload_root(root: &Path) -> UpdateResult<PathBuf> {
    for entry in fs::read_dir(root).map_err(|err| UpdateError {
        class: UpdateErrorClass::PayloadInvalid,
        detail: err.to_string(),
    })? {
        let path = entry
            .map_err(|err| UpdateError {
                class: UpdateErrorClass::PayloadInvalid,
                detail: err.to_string(),
            })?
            .path();
        if path.join("bin").is_dir() && path.join("share/fkst").is_dir() {
            return Ok(path);
        }
    }
    Err(UpdateError {
        class: UpdateErrorClass::PayloadInvalid,
        detail: "archive payload root not found".to_string(),
    })
}

fn validate_payload(root: &Path) -> UpdateResult<()> {
    for rel in [
        "bin/fkst-supervisor",
        "bin/fkst-framework",
        "bin/fkst",
        "share/fkst/VERSION",
    ] {
        if !root.join(rel).exists() {
            return Err(UpdateError {
                class: UpdateErrorClass::PayloadInvalid,
                detail: format!("payload missing {rel}"),
            });
        }
    }
    if !root.join("share/fkst").is_dir() {
        return Err(UpdateError {
            class: UpdateErrorClass::PayloadInvalid,
            detail: "payload missing share/fkst".to_string(),
        });
    }
    Ok(())
}

fn copy_payload(src: &Path, dst: &Path) -> UpdateResult<()> {
    let next = dst.with_extension(format!("next.{}", std::process::id()));
    let _ = fs::remove_dir_all(&next);
    fs::create_dir_all(&next).map_err(|err| UpdateError {
        class: UpdateErrorClass::SwapFailed,
        detail: err.to_string(),
    })?;
    let result = copy_payload_tree(src, &next).and_then(|_| {
        fs::rename(&next, dst).map_err(|err| UpdateError {
            class: UpdateErrorClass::SwapFailed,
            detail: err.to_string(),
        })
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&next);
    }
    result
}

fn copy_payload_tree(src: &Path, dst: &Path) -> UpdateResult<()> {
    copy_dir_all(&src.join("bin"), &dst.join("bin"))?;
    copy_dir_all(&src.join("share"), &dst.join("share"))?;
    if src.join("install.sh").exists() {
        fs::copy(src.join("install.sh"), dst.join("install.sh")).map_err(|err| UpdateError {
            class: UpdateErrorClass::SwapFailed,
            detail: err.to_string(),
        })?;
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> UpdateResult<()> {
    fs::create_dir_all(dst).map_err(|err| UpdateError {
        class: UpdateErrorClass::SwapFailed,
        detail: err.to_string(),
    })?;
    for entry in fs::read_dir(src).map_err(|err| UpdateError {
        class: UpdateErrorClass::SwapFailed,
        detail: err.to_string(),
    })? {
        let entry = entry.map_err(|err| UpdateError {
            class: UpdateErrorClass::SwapFailed,
            detail: err.to_string(),
        })?;
        let ty = entry.file_type().map_err(|err| UpdateError {
            class: UpdateErrorClass::SwapFailed,
            detail: err.to_string(),
        })?;
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(|err| UpdateError {
                class: UpdateErrorClass::SwapFailed,
                detail: err.to_string(),
            })?;
        }
    }
    Ok(())
}

fn atomic_current_swap(install_root: &Path, version: &str) -> UpdateResult<()> {
    let next = install_root.join("current.next");
    let current = install_root.join("current");
    let _ = fs::remove_file(&next);
    #[cfg(unix)]
    std::os::unix::fs::symlink(version, &next).map_err(|err| UpdateError {
        class: UpdateErrorClass::SwapFailed,
        detail: err.to_string(),
    })?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(version, &next).map_err(|err| UpdateError {
        class: UpdateErrorClass::SwapFailed,
        detail: err.to_string(),
    })?;
    fs::rename(&next, &current).map_err(|err| {
        let _ = fs::remove_file(&next);
        UpdateError {
            class: UpdateErrorClass::SwapFailed,
            detail: err.to_string(),
        }
    })
}
