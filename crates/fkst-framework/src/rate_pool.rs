//! Named external-command token bucket pools.
//!
//! Pool definitions come from `FKST_RATE_POOL_<NAME>=<burst>,<refill_per_minute>`.
//! The bucket ledger is locked with `flock`, so independent framework processes
//! share one host-stable command posture.

use anyhow::{anyhow, bail, Context, Result};
use nix::fcntl::{flock, FlockArg};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config_registry::{ConfigContext, ConfigKey, ConfigKind};

const ENV_PREFIX: &str = "FKST_RATE_POOL_";
const ROOT_ENV: &str = "FKST_RATE_POOL_ROOT";
const NANOS_PER_MINUTE: u128 = 60_000_000_000;
const INITIAL_WAIT: Duration = Duration::from_millis(20);
const MAX_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RatePoolConfig {
    pub(crate) burst: u64,
    pub(crate) refill_per_minute: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RatePoolRegistry {
    root: PathBuf,
    pools: BTreeMap<String, RatePoolConfig>,
}

#[derive(Clone, Copy, Debug)]
struct BucketState {
    tokens: u64,
    remainder_numerator: u128,
    updated_nanos: u128,
}

trait Clock {
    fn now_nanos(&self) -> u128;
}

trait Sleeper {
    fn sleep(&self, duration: Duration);
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_nanos(&self) -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }
}

struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }
}

impl RatePoolRegistry {
    pub(crate) fn from_config(config: &ConfigContext) -> Result<Self> {
        let pools = parse_pool_definitions(config.rate_pool_env())?;
        let root = expand_home(&config.resolved_string(ConfigKey::RatePoolRoot)?)?;
        let root = if root.is_absolute() {
            root
        } else {
            config.host_root().join(root)
        };
        let root = canonicalize_configured_root(root, pools.is_empty())?;
        Ok(Self { root, pools })
    }

    pub(crate) fn from_env() -> Result<Self> {
        let root = match std::env::var(ROOT_ENV) {
            Ok(value) if !value.trim().is_empty() => expand_home(value.trim())?,
            Ok(_) | Err(std::env::VarError::NotPresent) => {
                let ConfigKind::Operational { default } =
                    crate::config_registry::entry(ConfigKey::RatePoolRoot).kind
                else {
                    bail!("{ROOT_ENV} has no operational default");
                };
                expand_home(default)?
            }
            Err(std::env::VarError::NotUnicode(_)) => bail!("{ROOT_ENV} must be valid UTF-8"),
        };
        let mut values = BTreeMap::new();
        for (key, value) in std::env::vars_os() {
            let Some(key) = key.to_str() else {
                continue;
            };
            if key.starts_with(ENV_PREFIX) && key != ROOT_ENV {
                let Some(value) = value.to_str() else {
                    bail!("{key} must be valid UTF-8");
                };
                values.insert(key.to_string(), value.to_string());
            }
        }
        let pools = parse_pool_definitions(values)?;
        let root = canonicalize_configured_root(root, pools.is_empty())?;
        Ok(Self { root, pools })
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf, pools: BTreeMap<String, RatePoolConfig>) -> Self {
        Self { root, pools }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn pools(&self) -> &BTreeMap<String, RatePoolConfig> {
        &self.pools
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    pub(crate) fn acquire_for_program(&self, program: &str) -> Result<bool> {
        let Some(name) = pool_name_for_program(program) else {
            return Ok(false);
        };
        self.acquire_for_name(&name)
    }

    pub(crate) fn acquire_for_command_text(&self, command: &str) -> Result<bool> {
        let Some(program) = first_shell_word(command) else {
            return Ok(false);
        };
        self.acquire_for_program(&program)
    }

    pub(crate) fn acquire_for_name(&self, name: &str) -> Result<bool> {
        let normalized = name.to_ascii_lowercase();
        let Some(config) = self.pools.get(&normalized) else {
            return Ok(false);
        };
        acquire_token(
            &self.root,
            &normalized,
            config,
            &SystemClock,
            &ThreadSleeper,
        )?;
        Ok(true)
    }
}

pub(crate) fn parse_pool_definitions(
    values: BTreeMap<String, String>,
) -> Result<BTreeMap<String, RatePoolConfig>> {
    let mut pools = BTreeMap::new();
    for (env_key, raw) in values {
        let Some(name) = env_key.strip_prefix(ENV_PREFIX) else {
            continue;
        };
        if name == "ROOT" {
            continue;
        }
        validate_pool_name(name).with_context(|| format!("{env_key} has invalid pool name"))?;
        let config = parse_pool_config(&env_key, &raw)?;
        pools.insert(name.to_ascii_lowercase(), config);
    }
    Ok(pools)
}

fn parse_pool_config(env_key: &str, raw: &str) -> Result<RatePoolConfig> {
    let parts = raw.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() != 2 {
        bail!("{env_key} must use '<burst>,<refill_per_minute>', got {raw:?}");
    }
    let burst = parse_positive_u64(env_key, "burst", parts[0])?;
    let refill_per_minute = parse_positive_u64(env_key, "refill_per_minute", parts[1])?;
    Ok(RatePoolConfig {
        burst,
        refill_per_minute,
    })
}

fn parse_positive_u64(env_key: &str, label: &str, raw: &str) -> Result<u64> {
    let value = raw
        .parse::<u64>()
        .with_context(|| format!("{env_key} {label} must be a positive integer, got {raw:?}"))?;
    if value == 0 {
        bail!("{env_key} {label} must be > 0");
    }
    Ok(value)
}

fn validate_pool_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_'))
    {
        bail!("pool names must match [A-Z0-9_]+");
    }
    Ok(())
}

fn expand_home(raw: &str) -> Result<PathBuf> {
    if raw == "~" || raw.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("{ROOT_ENV} uses '~' but HOME is not set"))?;
        let mut path = PathBuf::from(home);
        if raw.len() > 2 {
            path.push(&raw[2..]);
        }
        Ok(path)
    } else {
        Ok(PathBuf::from(raw))
    }
}

fn canonicalize_configured_root(root: PathBuf, empty: bool) -> Result<PathBuf> {
    if empty {
        return Ok(root);
    }
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create rate pool root {}", root.display()))?;
    root.canonicalize()
        .with_context(|| format!("canonicalize rate pool root {}", root.display()))
}

fn pool_name_for_program(program: &str) -> Option<String> {
    Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| name.to_ascii_lowercase())
}

fn first_shell_word(command: &str) -> Option<String> {
    let trimmed = command.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let mut word = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' => {
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    word.push(inner);
                }
            }
            '"' => {
                while let Some(inner) = chars.next() {
                    if inner == '"' {
                        break;
                    }
                    if inner == '\\' {
                        if let Some(escaped) = chars.next() {
                            word.push(escaped);
                        }
                    } else {
                        word.push(inner);
                    }
                }
            }
            '\\' => {
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            ch if ch.is_whitespace() => break,
            _ => word.push(ch),
        }
    }
    if word.is_empty() {
        None
    } else {
        Some(word)
    }
}

fn acquire_token(
    root: &Path,
    name: &str,
    config: &RatePoolConfig,
    clock: &impl Clock,
    sleeper: &impl Sleeper,
) -> Result<()> {
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let mut wait = INITIAL_WAIT;
    loop {
        if try_acquire_token(root, name, config, clock)? {
            return Ok(());
        }
        sleeper.sleep(wait);
        wait = (wait * 2).min(MAX_WAIT);
    }
}

fn try_acquire_token(
    root: &Path,
    name: &str,
    config: &RatePoolConfig,
    clock: &impl Clock,
) -> Result<bool> {
    std::fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let ledger_path = root.join(format!("{name}.bucket"));
    let lock_path = root.join(format!("{name}.lock"));
    let lock = open_lock(&lock_path)?;
    flock(lock.as_raw_fd(), FlockArg::LockExclusive)
        .with_context(|| format!("lock {}", lock_path.display()))?;
    let now = clock.now_nanos();
    let mut state = read_state(&ledger_path, now, config);
    state = refill(state, config, now);
    let admitted = if state.tokens > 0 {
        state.tokens -= 1;
        true
    } else {
        false
    };
    write_state(&ledger_path, state)?;
    Ok(admitted)
}

fn open_lock(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn empty_state(now_nanos: u128) -> BucketState {
    BucketState {
        tokens: 0,
        remainder_numerator: 0,
        updated_nanos: now_nanos,
    }
}

fn read_state(path: &Path, now_nanos: u128, config: &RatePoolConfig) -> BucketState {
    let Ok(mut file) = File::open(path) else {
        return empty_state(now_nanos);
    };
    let mut content = String::new();
    if file.read_to_string(&mut content).is_err() {
        return empty_state(now_nanos);
    }
    if content.trim().is_empty() {
        return empty_state(now_nanos);
    }
    let mut lines = content.lines();
    let parsed = (|| -> Result<BucketState> {
        let updated_nanos = parse_state_field(lines.next(), "updated_nanos")?;
        let tokens: u64 = parse_state_field(lines.next(), "tokens")?;
        let remainder_numerator: u128 = parse_state_field(lines.next(), "remainder_nanos")?;
        Ok(BucketState {
            tokens: tokens.min(config.burst),
            remainder_numerator,
            updated_nanos,
        })
    })();
    parsed.unwrap_or_else(|_| empty_state(now_nanos))
}

fn parse_state_field<T>(line: Option<&str>, label: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let line = line.ok_or_else(|| anyhow!("rate pool ledger missing {label}"))?;
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| anyhow!("rate pool ledger field must use key=value"))?;
    if key != label {
        bail!("rate pool ledger expected {label}, got {key}");
    }
    value
        .parse::<T>()
        .with_context(|| format!("parse rate pool ledger {label}"))
}

fn write_state(path: &Path, state: BucketState) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("rate pool ledger '{}' has no parent", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temp_path = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("bucket"),
        std::process::id(),
        ulid::Ulid::new()
    ));
    {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("create {}", temp_path.display()))?;
        write!(
            file,
            "updated_nanos={}\ntokens={}\nremainder_nanos={}\n",
            state.updated_nanos, state.tokens, state.remainder_numerator
        )?;
        file.sync_all()
            .with_context(|| format!("sync {}", temp_path.display()))?;
    }
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("rename {} to {}", temp_path.display(), path.display()))?;
    let dir = File::open(parent).with_context(|| format!("open {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("sync {}", parent.display()))?;
    Ok(())
}

#[cfg(test)]
fn seed_state(path: &Path, state: BucketState) -> Result<()> {
    write_state(path, state)
}

#[cfg(test)]
fn ledger_body(path: &Path) -> Result<String> {
    let mut body = String::new();
    File::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .read_to_string(&mut body)
        .with_context(|| format!("read {}", path.display()))?;
    Ok(body)
}

#[cfg(test)]
fn parse_tokens(body: &str) -> u64 {
    body.lines()
        .find_map(|line| line.strip_prefix("tokens="))
        .expect("ledger must contain tokens")
        .parse::<u64>()
        .expect("tokens must parse")
}

#[cfg(test)]
fn parse_remainder(body: &str) -> u128 {
    body.lines()
        .find_map(|line| line.strip_prefix("remainder_nanos="))
        .expect("ledger must contain remainder")
        .parse::<u128>()
        .expect("remainder must parse")
}

#[cfg(test)]
fn parse_updated(body: &str) -> u128 {
    body.lines()
        .find_map(|line| line.strip_prefix("updated_nanos="))
        .expect("ledger must contain updated_nanos")
        .parse::<u128>()
        .expect("updated_nanos must parse")
}

fn refill(mut state: BucketState, config: &RatePoolConfig, now_nanos: u128) -> BucketState {
    if now_nanos < state.updated_nanos {
        state.updated_nanos = now_nanos;
        return state;
    }
    if now_nanos == state.updated_nanos {
        return state;
    }
    if state.tokens >= config.burst {
        state.tokens = config.burst;
        state.remainder_numerator = 0;
        state.updated_nanos = now_nanos;
        return state;
    }

    let elapsed = now_nanos - state.updated_nanos;
    let produced_numerator = elapsed
        .saturating_mul(config.refill_per_minute as u128)
        .saturating_add(state.remainder_numerator);
    let produced = produced_numerator / NANOS_PER_MINUTE;
    state.remainder_numerator = produced_numerator % NANOS_PER_MINUTE;
    if produced > 0 {
        let produced = produced.min(u64::MAX as u128) as u64;
        state.tokens = state.tokens.saturating_add(produced).min(config.burst);
        if state.tokens >= config.burst {
            state.remainder_numerator = 0;
        }
    }
    state.updated_nanos = now_nanos;
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier, Mutex,
    };

    struct ManualClock {
        now: Mutex<u128>,
    }

    impl ManualClock {
        fn new(now: u128) -> Self {
            Self {
                now: Mutex::new(now),
            }
        }
    }

    impl Clock for ManualClock {
        fn now_nanos(&self) -> u128 {
            *self.now.lock().unwrap()
        }
    }

    struct AdvancingSleeper {
        clock: Arc<ManualClock>,
        step: Duration,
        sleeps: AtomicUsize,
    }

    impl AdvancingSleeper {
        fn new(clock: Arc<ManualClock>, step: Duration) -> Self {
            Self {
                clock,
                step,
                sleeps: AtomicUsize::new(0),
            }
        }
    }

    impl Sleeper for AdvancingSleeper {
        fn sleep(&self, _duration: Duration) {
            self.sleeps.fetch_add(1, Ordering::SeqCst);
            let mut now = self.clock.now.lock().unwrap();
            *now += self.step.as_nanos();
        }
    }

    fn state(tokens: u64, remainder_numerator: u128, updated_nanos: u128) -> BucketState {
        BucketState {
            tokens,
            remainder_numerator,
            updated_nanos,
        }
    }

    fn run_process_child_if_requested() {
        if std::env::var_os("FKST_RATE_POOL_CHILD_ACQUIRE").is_none() {
            return;
        }
        let root = PathBuf::from(std::env::var_os("FKST_RATE_POOL_CHILD_ROOT").unwrap());
        let admitted = try_acquire_token(
            &root,
            "gh",
            &RatePoolConfig {
                burst: 2,
                refill_per_minute: 60,
            },
            &ManualClock::new(0),
        )
        .unwrap();
        std::process::exit(if admitted { 0 } else { 3 });
    }

    #[test]
    fn parse_pool_definitions_fails_closed_on_invalid_format() {
        run_process_child_if_requested();
        let err = parse_pool_definitions(BTreeMap::from([(
            "FKST_RATE_POOL_GH".to_string(),
            "50".to_string(),
        )]))
        .unwrap_err();
        assert!(format!("{err:#}").contains("FKST_RATE_POOL_GH must use"));
    }

    #[test]
    fn refill_math_tracks_numerator_remainder_exactly() {
        run_process_child_if_requested();
        let config = RatePoolConfig {
            burst: 3,
            refill_per_minute: 120,
        };
        let state = refill(state(0, 0, 0), &config, 500_000_000);
        assert_eq!(state.tokens, 1);
        assert_eq!(state.remainder_numerator, 0);
        assert_eq!(state.updated_nanos, 500_000_000);

        let state = refill(state, &config, 1_000_000_000);
        assert_eq!(state.tokens, 2);
        assert_eq!(state.remainder_numerator, 0);
        assert_eq!(state.updated_nanos, 1_000_000_000);
    }

    #[test]
    fn refill_math_does_not_mint_bonus_token_after_exact_boundary() {
        run_process_child_if_requested();
        let config = RatePoolConfig {
            burst: 3,
            refill_per_minute: 6,
        };
        let state = refill(state(0, 0, 0), &config, 9_999_999_999);
        assert_eq!(state.tokens, 0);
        assert_eq!(state.remainder_numerator, 59_999_999_994);

        let state = refill(state, &config, 10_000_000_000);
        assert_eq!(state.tokens, 1);
        assert_eq!(state.remainder_numerator, 0);

        let state = refill(BucketState { tokens: 0, ..state }, &config, 10_000_000_001);
        assert_eq!(state.tokens, 0);
        assert_eq!(state.remainder_numerator, 6);
    }

    #[test]
    fn acquire_blocks_until_refill_without_wall_clock_sleep() {
        run_process_child_if_requested();
        let tmp = tempfile::tempdir().unwrap();
        let config = RatePoolConfig {
            burst: 1,
            refill_per_minute: 60,
        };
        let clock = Arc::new(ManualClock::new(0));
        let sleeper = AdvancingSleeper::new(clock.clone(), Duration::from_secs(1));
        seed_state(&tmp.path().join("gh.bucket"), state(1, 0, 0)).unwrap();

        acquire_token(tmp.path(), "gh", &config, clock.as_ref(), &sleeper).unwrap();
        assert_eq!(sleeper.sleeps.load(Ordering::SeqCst), 0);

        acquire_token(tmp.path(), "gh", &config, clock.as_ref(), &sleeper).unwrap();
        assert_eq!(sleeper.sleeps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn unreadable_or_corrupt_ledger_is_empty_not_burst() {
        run_process_child_if_requested();
        let tmp = tempfile::tempdir().unwrap();
        let config = RatePoolConfig {
            burst: 2,
            refill_per_minute: 60,
        };
        let clock = ManualClock::new(0);
        std::fs::write(tmp.path().join("gh.bucket"), "not a ledger\n").unwrap();

        assert!(!try_acquire_token(tmp.path(), "gh", &config, &clock).unwrap());
        let body = ledger_body(&tmp.path().join("gh.bucket")).unwrap();
        assert_eq!(parse_tokens(&body), 0);
        assert_eq!(parse_remainder(&body), 0);
        assert_eq!(parse_updated(&body), 0);
    }

    #[test]
    fn backwards_clock_clamps_updated_without_stalling_future_refill() {
        run_process_child_if_requested();
        let config = RatePoolConfig {
            burst: 2,
            refill_per_minute: 60,
        };
        let state = refill(state(0, 0, 2_000_000_000), &config, 500_000_000);
        assert_eq!(state.tokens, 0);
        assert_eq!(state.remainder_numerator, 0);
        assert_eq!(state.updated_nanos, 500_000_000);

        let state = refill(state, &config, 1_500_000_000);
        assert_eq!(state.tokens, 1);
        assert_eq!(state.remainder_numerator, 0);
    }

    #[test]
    fn concurrent_try_acquire_admits_exact_seeded_token_count() {
        run_process_child_if_requested();
        let tmp = tempfile::tempdir().unwrap();
        let config = RatePoolConfig {
            burst: 2,
            refill_per_minute: 60,
        };
        seed_state(&tmp.path().join("gh.bucket"), state(2, 0, 0)).unwrap();
        let clock = Arc::new(ManualClock::new(0));
        let barrier = Arc::new(Barrier::new(9));
        let admitted = Arc::new(AtomicUsize::new(0));

        let handles = (0..8)
            .map(|_| {
                let root = tmp.path().to_path_buf();
                let config = config.clone();
                let clock = clock.clone();
                let barrier = barrier.clone();
                let admitted = admitted.clone();
                thread::spawn(move || {
                    barrier.wait();
                    if try_acquire_token(&root, "gh", &config, clock.as_ref()).unwrap() {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect::<Vec<_>>();

        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(admitted.load(Ordering::SeqCst), 2);
        let body = ledger_body(&tmp.path().join("gh.bucket")).unwrap();
        assert_eq!(parse_tokens(&body), 0);
    }

    #[test]
    fn separate_processes_share_locked_bucket_admission_count() {
        run_process_child_if_requested();
        let tmp = tempfile::tempdir().unwrap();
        seed_state(&tmp.path().join("gh.bucket"), state(2, 0, 0)).unwrap();
        let exe = std::env::current_exe().unwrap();
        let current_test = std::thread::current().name().unwrap().to_string();
        let statuses = (0..4)
            .map(|_| {
                Command::new(&exe)
                    .arg(&current_test)
                    .arg("--exact")
                    .arg("--nocapture")
                    .env("FKST_RATE_POOL_CHILD_ACQUIRE", "1")
                    .env("FKST_RATE_POOL_CHILD_ROOT", tmp.path())
                    .status()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let admitted = statuses
            .iter()
            .filter(|status| status.code() == Some(0))
            .count();
        let denied = statuses
            .iter()
            .filter(|status| status.code() == Some(3))
            .count();
        assert_eq!(admitted, 2, "{statuses:?}");
        assert_eq!(denied, 2, "{statuses:?}");
        let body = ledger_body(&tmp.path().join("gh.bucket")).unwrap();
        assert_eq!(parse_tokens(&body), 0);
    }

    #[test]
    fn command_matching_uses_program_basename_and_shell_first_word() {
        run_process_child_if_requested();
        let registry = RatePoolRegistry {
            root: PathBuf::from("/tmp/unused"),
            pools: BTreeMap::from([(
                "gh".to_string(),
                RatePoolConfig {
                    burst: 1,
                    refill_per_minute: 1,
                },
            )]),
        };
        assert_eq!(pool_name_for_program("/usr/bin/gh").unwrap(), "gh");
        assert_eq!(pool_name_for_program("/usr/local/bin/GH").unwrap(), "gh");
        assert_eq!(first_shell_word("  gh issue list").unwrap(), "gh");
        assert_eq!(
            first_shell_word("  /usr/bin/GH issue list").unwrap(),
            "/usr/bin/GH"
        );
        assert_eq!(first_shell_word("'gh' issue list").unwrap(), "gh");
        assert!(registry.pools.contains_key("gh"));
    }
}
