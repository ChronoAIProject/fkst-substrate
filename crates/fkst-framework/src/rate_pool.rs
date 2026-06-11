//! Named external-command token bucket pools.
//!
//! Pool definitions come from `FKST_RATE_POOL_<NAME>=<burst>,<refill_per_minute>`.
//! The bucket ledger is locked with `flock`, so independent framework processes
//! share one host-stable command posture.

use anyhow::{anyhow, bail, Context, Result};
use nix::fcntl::{flock, FlockArg};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config_registry::{ConfigContext, ConfigKey};

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
    remainder_nanos: u128,
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
        let root = expand_home(&config.resolved_string(ConfigKey::RatePoolRoot)?)?;
        let pools = parse_pool_definitions(config.rate_pool_env())?;
        Ok(Self { root, pools })
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: PathBuf, pools: BTreeMap<String, RatePoolConfig>) -> Self {
        Self { root, pools }
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

    fn acquire_for_name(&self, name: &str) -> Result<bool> {
        let Some(config) = self.pools.get(name) else {
            return Ok(false);
        };
        acquire_token(&self.root, name, config, &SystemClock, &ThreadSleeper)?;
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
    let path = root.join(format!("{name}.bucket"));
    let mut wait = INITIAL_WAIT;
    loop {
        let mut file = open_ledger(&path)?;
        flock(file.as_raw_fd(), FlockArg::LockExclusive)
            .with_context(|| format!("lock {}", path.display()))?;
        let now = clock.now_nanos();
        let mut state = read_state(&mut file, now, config)?;
        state = refill(state, config, now);
        if state.tokens > 0 {
            state.tokens -= 1;
            write_state(&mut file, state)?;
            return Ok(());
        }
        write_state(&mut file, state)?;
        drop(file);

        sleeper.sleep(wait);
        wait = (wait * 2).min(MAX_WAIT);
    }
}

fn open_ledger(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn read_state(file: &mut File, now_nanos: u128, config: &RatePoolConfig) -> Result<BucketState> {
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    file.read_to_string(&mut content)?;
    if content.trim().is_empty() {
        return Ok(BucketState {
            tokens: config.burst,
            remainder_nanos: 0,
            updated_nanos: now_nanos,
        });
    }
    let mut lines = content.lines();
    let updated_nanos = parse_state_field(lines.next(), "updated_nanos")?;
    let tokens = parse_state_field(lines.next(), "tokens")?;
    let remainder_nanos = parse_state_field(lines.next(), "remainder_nanos")?;
    Ok(BucketState {
        tokens,
        remainder_nanos,
        updated_nanos,
    })
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

fn write_state(file: &mut File, state: BucketState) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    write!(
        file,
        "updated_nanos={}\ntokens={}\nremainder_nanos={}\n",
        state.updated_nanos, state.tokens, state.remainder_nanos
    )?;
    file.sync_all()?;
    Ok(())
}

fn refill(mut state: BucketState, config: &RatePoolConfig, now_nanos: u128) -> BucketState {
    if now_nanos <= state.updated_nanos {
        return state;
    }
    if state.tokens >= config.burst {
        state.tokens = config.burst;
        state.remainder_nanos = 0;
        state.updated_nanos = now_nanos;
        return state;
    }

    let elapsed = now_nanos - state.updated_nanos;
    let available_nanos = elapsed.saturating_add(state.remainder_nanos);
    let produced_numerator = available_nanos.saturating_mul(config.refill_per_minute as u128);
    let produced = produced_numerator / NANOS_PER_MINUTE;
    state.remainder_nanos = available_nanos % NANOS_PER_MINUTE;
    if produced > 0 {
        let produced = produced.min(u64::MAX as u128) as u64;
        state.tokens = state.tokens.saturating_add(produced).min(config.burst);
        if state.tokens >= config.burst {
            state.remainder_nanos = 0;
        }
    }
    state.updated_nanos = now_nanos;
    state
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn parse_pool_definitions_fails_closed_on_invalid_format() {
        let err = parse_pool_definitions(BTreeMap::from([(
            "FKST_RATE_POOL_GH".to_string(),
            "50".to_string(),
        )]))
        .unwrap_err();
        assert!(format!("{err:#}").contains("FKST_RATE_POOL_GH must use"));
    }

    #[test]
    fn refill_math_respects_boundaries_and_burst_cap() {
        let config = RatePoolConfig {
            burst: 3,
            refill_per_minute: 6,
        };
        let state = BucketState {
            tokens: 0,
            remainder_nanos: 0,
            updated_nanos: 0,
        };
        let state = refill(state, &config, 9_999_999_999);
        assert_eq!(state.tokens, 0);
        assert!(state.remainder_nanos > 0);

        let state = refill(state, &config, 10_000_000_000);
        assert_eq!(state.tokens, 1);

        let state = refill(state, &config, 70_000_000_000);
        assert_eq!(state.tokens, 3);
        assert_eq!(state.remainder_nanos, 0);
    }

    #[test]
    fn acquire_blocks_until_refill_without_wall_clock_sleep() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RatePoolConfig {
            burst: 1,
            refill_per_minute: 60,
        };
        let clock = Arc::new(ManualClock::new(0));
        let sleeper = AdvancingSleeper::new(clock.clone(), Duration::from_secs(1));

        acquire_token(tmp.path(), "gh", &config, clock.as_ref(), &sleeper).unwrap();
        assert_eq!(sleeper.sleeps.load(Ordering::SeqCst), 0);

        acquire_token(tmp.path(), "gh", &config, clock.as_ref(), &sleeper).unwrap();
        assert_eq!(sleeper.sleeps.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn concurrent_acquire_serializes_through_locked_ledger() {
        let tmp = tempfile::tempdir().unwrap();
        let config = RatePoolConfig {
            burst: 2,
            refill_per_minute: 120,
        };
        let clock = Arc::new(ManualClock::new(0));
        let sleeper = Arc::new(AdvancingSleeper::new(clock.clone(), Duration::from_secs(1)));
        let barrier = Arc::new(Barrier::new(4));

        let handles = (0..4)
            .map(|_| {
                let root = tmp.path().to_path_buf();
                let config = config.clone();
                let clock = clock.clone();
                let sleeper = sleeper.clone();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    acquire_token(&root, "gh", &config, clock.as_ref(), sleeper.as_ref()).unwrap();
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let mut ledger = String::new();
        File::open(tmp.path().join("gh.bucket"))
            .unwrap()
            .read_to_string(&mut ledger)
            .unwrap();
        let tokens = ledger
            .lines()
            .find_map(|line| line.strip_prefix("tokens="))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(tokens <= config.burst, "{ledger}");
    }

    #[test]
    fn command_matching_uses_program_basename_and_shell_first_word() {
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
        assert_eq!(first_shell_word("  gh issue list").unwrap(), "gh");
        assert_eq!(first_shell_word("'gh' issue list").unwrap(), "gh");
        assert!(registry.pools.contains_key("gh"));
    }
}
