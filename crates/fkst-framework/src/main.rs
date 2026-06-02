//! fkst-framework — Tier III one-shot Lua runner.
//!
//! CLI: `fkst-framework run <lua_file> --event '<json>'`
//! CLI: `fkst-framework supervise --project-root <path> --framework-bin <path>`
//! CLI: `fkst-framework known-good <promote|bootstrap> [options]`
//! CLI: `fkst-framework conformance --project-root <path>`
//! CLI: `fkst-framework update [options]`
//! CLI: `fkst-framework --self-test`
//! Exit codes:
//!   0 = pipeline ok
//!   1 = lua error / pipeline error
//!   2 = SDK / IO / arg parsing error
//!   124 = no-output stall killed by SIGKILL -pgid

use anyhow::{Context, Result};
use host_conformance::HostConformanceOptions;
use known_good::{HealthGateOptions, KnownGoodRef, PromotionOutcome, SwapEvidence};
use path_resolver::PackageRoots;
use serde_json::Value as JsonValue;
use std::path::PathBuf;
use update::UpdateCli;

mod host_conformance;
mod known_good;
mod mlua_init;
mod path_resolver;
mod raise;
mod sdk_basic;
mod sdk_codex;
mod sdk_fs;
mod sdk_git;
mod sdk_log;
mod self_test;
mod supervise;
mod update;

use raise::RaiseBuffer;

enum CliCommand {
    Run {
        lua_path: PathBuf,
        package_root: PathBuf,
        event: JsonValue,
    },
    Supervise {
        roots: PackageRoots,
        framework_bin: PathBuf,
    },
    KnownGood(KnownGoodCli),
    Conformance(HostConformanceOptions),
    Update(UpdateCli),
    SelfTest,
}

fn parse_args() -> Result<CliCommand> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut args_iter = args.into_iter();
    let sub = args_iter.next().ok_or_else(|| {
        anyhow::anyhow!(
            "usage: fkst-framework run <lua> --event <json> | fkst-framework supervise --project-root <path> --framework-bin <path> | fkst-framework known-good <promote|bootstrap> [options] | fkst-framework --self-test"
        )
    })?;
    if sub == "--self-test" {
        if let Some(other) = args_iter.next() {
            anyhow::bail!("unknown --self-test option: {}", other);
        }
        return Ok(CliCommand::SelfTest);
    }
    if sub == "supervise" {
        let mut project_root: Option<PathBuf> = None;
        let mut package_root: Option<PathBuf> = None;
        let mut framework_bin: Option<PathBuf> = None;
        while let Some(a) = args_iter.next() {
            match a.as_str() {
                "--project-root" => project_root = args_iter.next().map(PathBuf::from),
                "--package-root" => package_root = args_iter.next().map(PathBuf::from),
                "--framework-bin" => framework_bin = args_iter.next().map(PathBuf::from),
                other => anyhow::bail!("unknown supervise argument: {}", other),
            }
        }
        let project_root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
        return Ok(CliCommand::Supervise {
            roots: PackageRoots::resolve(project_root, package_root)?,
            framework_bin: framework_bin
                .ok_or_else(|| anyhow::anyhow!("missing --framework-bin"))?,
        });
    }
    if sub == "known-good" {
        // Tier III framework CLI owns known-good CAS,
        // health evidence, rollback witness, and ref maintenance.
        let action = args_iter
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!("usage: fkst-framework known-good <promote|bootstrap> [options]")
            })?
            .to_string();
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::KnownGood(KnownGoodCli::parse(
            action,
            rest.as_slice(),
        )?));
    }
    if sub == "conformance" {
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::Conformance(parse_conformance_args(&rest)?));
    }
    if sub == "update" {
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::Update(UpdateCli::parse(&rest)?));
    }
    if sub == "run" {
        let lua_path: PathBuf = args_iter
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing <lua_file> arg"))?
            .into();
        let mut package_root: Option<PathBuf> = None;
        let mut event_json: Option<String> = None;
        while let Some(a) = args_iter.next() {
            match a.as_str() {
                "--event" => event_json = args_iter.next(),
                "--package-root" => package_root = args_iter.next().map(PathBuf::from),
                other => anyhow::bail!("unknown run argument: {}", other),
            }
        }
        let event_str = event_json.unwrap_or_else(|| "{}".into());
        let event: JsonValue = serde_json::from_str(&event_str)
            .with_context(|| format!("--event not valid json: {}", event_str))?;
        let inferred_project_root = mlua_init::package_root_for_lua(&lua_path);
        let roots = PackageRoots::resolve(inferred_project_root, package_root)?;
        return Ok(CliCommand::Run {
            lua_path,
            package_root: roots.package_root().to_path_buf(),
            event,
        });
    }
    anyhow::bail!("unknown subcommand: {}", sub);
}

fn parse_conformance_args(args: &[String]) -> Result<HostConformanceOptions> {
    let mut project_root: Option<PathBuf> = None;
    let mut package_root: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--project-root" => {
                if project_root.is_some() {
                    anyhow::bail!("duplicate --project-root");
                }
                i += 1;
                project_root = Some(next_value(args, i, "--project-root")?.into());
            }
            "--package-root" => {
                if package_root.is_some() {
                    anyhow::bail!("duplicate --package-root");
                }
                i += 1;
                package_root = Some(next_value(args, i, "--package-root")?.into());
            }
            other => anyhow::bail!("unknown conformance argument: {}", other),
        }
        i += 1;
    }

    let root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
    Ok(HostConformanceOptions {
        roots: PackageRoots::resolve(root, package_root)?,
    })
}

#[derive(Default)]
struct KnownGoodCli {
    action: String,
    project_root: Option<PathBuf>,
    integration_ref: Option<String>,
    review: Option<String>,
    health_options: HealthGateOptions,
}

impl KnownGoodCli {
    fn parse(action: String, args: &[String]) -> Result<Self> {
        let mut options = Self {
            action,
            ..Self::default()
        };
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--project-root" => {
                    i += 1;
                    options.project_root = Some(next_value(args, i, "--project-root")?.into());
                }
                "--integration-ref" => {
                    i += 1;
                    options.integration_ref = Some(next_value(args, i, "--integration-ref")?);
                }
                "--review" => {
                    i += 1;
                    options.review = Some(next_value(args, i, "--review")?);
                }
                "--health-window-seconds" => {
                    i += 1;
                    let seconds = next_value(args, i, "--health-window-seconds")?
                        .parse::<u64>()
                        .context("parse --health-window-seconds")?;
                    options.health_options.window = std::time::Duration::from_secs(seconds);
                }
                "--health-poll-ms" => {
                    i += 1;
                    let ms = next_value(args, i, "--health-poll-ms")?
                        .parse::<u64>()
                        .context("parse --health-poll-ms")?;
                    options.health_options.poll_interval = std::time::Duration::from_millis(ms);
                }
                "--supervisor-log" => {
                    i += 1;
                    options.health_options.supervisor_log =
                        Some(next_value(args, i, "--supervisor-log")?.into());
                }
                other => anyhow::bail!("unknown known-good option: {}", other),
            }
            i += 1;
        }
        Ok(options)
    }
}

fn next_value(args: &[String], index: usize, flag: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing {} value", flag))
}

fn run_known_good_command(options: KnownGoodCli) -> Result<i32> {
    let root = options
        .project_root
        .unwrap_or(std::env::current_dir().context("get cwd")?);
    let known_good = KnownGoodRef::new(root.clone(), options.integration_ref)?;
    let evidence = SwapEvidence {
        conformance: Some(run_known_good_conformance(&root)?),
        self_test: Some(run_known_good_self_test()),
        review: options.review,
    };
    let result = match options.action.as_str() {
        "promote" => {
            known_good.promote_with_health_options(&evidence, "reconcile", &options.health_options)
        }
        "bootstrap" => known_good.bootstrap(&evidence, "manual-bootstrap"),
        other => anyhow::bail!("unknown known-good action: {}", other),
    };

    match result {
        Ok(outcome) => {
            print_known_good_success(&outcome);
            Ok(0)
        }
        Err(err) => {
            if let Some(evidence_line) = err.health_evidence() {
                eprintln!("{evidence_line}");
            }
            eprintln!(
                "event=known-good-swap result={} error_class={}",
                err.class(),
                err.class()
            );
            Ok(2)
        }
    }
}

fn run_known_good_conformance(root: &std::path::Path) -> Result<String> {
    let code = host_conformance::run(HostConformanceOptions {
        roots: PackageRoots::resolve(root, None)?,
    })?;
    if code == 0 {
        Ok("host_conformance:pass".to_string())
    } else {
        Ok(format!("host_conformance:fail:{code}"))
    }
}

fn run_known_good_self_test() -> String {
    match self_test::run() {
        Ok(()) => "fkst-framework --self-test:pass".to_string(),
        Err(err) => format!("SELF_TEST_FAILED:{}: {:#}", err.class(), err.source()),
    }
}

fn print_known_good_success(outcome: &PromotionOutcome) {
    match outcome {
        PromotionOutcome::Promoted {
            integration_ref,
            old_known_good_sha,
            candidate_sha,
            health,
        } => {
            println!(
                "event=known-good-swap result=promoted integration_ref={} old_known_good={} candidate={} {}",
                integration_ref, old_known_good_sha, candidate_sha, health.evidence_line
            );
        }
        PromotionOutcome::Bootstrapped {
            integration_ref,
            candidate_sha,
        } => {
            println!(
                "event=known-good-swap result=bootstrapped trigger=manual-bootstrap integration_ref={} old_known_good=0000000000000000000000000000000000000000 candidate={}",
                integration_ref, candidate_sha
            );
        }
        PromotionOutcome::NoOp {
            integration_ref,
            sha,
        } => {
            println!(
                "event=known-good-swap result=known-good-swap-no-op integration_ref={} old_known_good={} candidate={}",
                integration_ref, sha, sha
            );
        }
    }
}

fn run_pipeline(lua_path: PathBuf, package_root: PathBuf, event: JsonValue) -> Result<i32> {
    let lua = mlua_init::new_lua();
    let raise_buf = RaiseBuffer::new();

    mlua_init::register_framework_sdk(&lua, raise_buf.clone())?;

    let exit_code =
        match mlua_init::run_dept_with_package_root(&lua, &lua_path, &event, &package_root) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("[framework] pipeline failed: {:#}", e);
                1
            }
        };

    raise_buf.emit_stdout();
    Ok(exit_code)
}

fn run() -> Result<i32> {
    match parse_args()? {
        CliCommand::Run {
            lua_path,
            package_root,
            event,
        } => run_pipeline(lua_path, package_root, event),
        CliCommand::Supervise {
            roots,
            framework_bin,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive(tracing::Level::INFO.into()),
                )
                .init();
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build supervise runtime")?;
            rt.block_on(supervise::supervise(roots, framework_bin))?;
            Ok(0)
        }
        CliCommand::KnownGood(options) => run_known_good_command(options),
        CliCommand::Conformance(options) => host_conformance::run(options),
        CliCommand::Update(options) => Ok(update::run_update_command(options)),
        CliCommand::SelfTest => match self_test::run() {
            Ok(()) => Ok(0),
            Err(err) => {
                eprintln!("SELF_TEST_FAILED:{}: {:#}", err.class(), err.source());
                Ok(2)
            }
        },
    }
}

fn main() {
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[framework] startup error: {:#}", e);
            2
        }
    };
    std::process::exit(code);
}
