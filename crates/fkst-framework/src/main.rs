//! fkst-framework — Tier III one-shot Lua runner.
//!
//! CLI: `fkst-framework run <lua_file> --project-root <path> --package-root <path> ... --owner-namespace <id> --event '<json>'`
//! Multiple `--package-root` flags give composed namespace knowledge; `--owner-namespace`
//! selects the owner root used for Lua `require`.
//! CLI: `fkst-framework supervise --project-root <path> --framework-bin <path>`
//! CLI: `fkst-framework conformance --project-root <path> [--package-root <path> ...] [--report-json <path>]`
//! CLI: `fkst-framework test --project-root <path> [--package-root <path> ...] [--report-json <path>]`
//! CLI: `fkst-framework init-package-repo [--ref <substrate-ref>] [--force]`
//! CLI: `fkst-framework --self-test`
//! Exit codes:
//!   0 = pipeline ok
//!   1 = lua error / pipeline error
//!   2 = SDK / IO / arg parsing error
//!   124 = codex subprocess timed out and was killed by SIGKILL -pgid

use anyhow::{Context, Result};
use host_conformance::HostConformanceOptions;
use path_resolver::PackageRoots;
use serde_json::Value as JsonValue;
use std::path::{Path, PathBuf};

mod boundary_resource;
mod config_registry;
mod external_command;
mod host_conformance;
mod init_package_repo;
mod mlua_init;
mod path_resolver;
mod process_tree;
mod raise;
mod rate_pool;
mod rate_shim;
mod runtime_context;
mod sdk_basic;
mod sdk_cache;
mod sdk_codex;
mod sdk_fs;
mod sdk_git;
mod sdk_graph;
mod sdk_i18n;
mod sdk_json;
mod sdk_log;
mod sdk_mark;
mod sdk_strings;
mod self_test;
mod supervise;
mod test_runner;

#[cfg(test)]
mod test_env {
    pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

use raise::RaiseBuffer;

enum CliCommand {
    Run {
        lua_path: PathBuf,
        roots: PackageRoots,
        owner_namespace: String,
        event: JsonValue,
    },
    Supervise {
        roots: PackageRoots,
        framework_bin: PathBuf,
    },
    Conformance(HostConformanceOptions),
    Config(ConfigCli),
    BoundaryResources,
    RateAcquire {
        pool: String,
    },
    Test(TestCli),
    InitPackageRepo(init_package_repo::InitPackageRepoOptions),
    SelfTest,
}

fn parse_args() -> Result<CliCommand> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut args_iter = args.into_iter();
    let sub = args_iter.next().ok_or_else(|| {
        anyhow::anyhow!(
            "usage: fkst-framework run <lua> --project-root <path> --package-root <path> [--package-root <path> ...] [--owner-namespace <id>] --event <json> | fkst-framework supervise --project-root <path> --framework-bin <path> [--package-root <path> ...] | fkst-framework conformance --project-root <path> [--package-root <path> ...] [--report-json <path>] | fkst-framework config --project-root <path> [--package-root <path> ...] | fkst-framework boundary-resources | fkst-framework rate-acquire <pool> | fkst-framework test --project-root <path> [--package-root <path> ...] [--report-json <path>] | fkst-framework init-package-repo [--ref <substrate-ref>] [--force] | fkst-framework --self-test"
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
        let mut package_roots: Vec<PathBuf> = Vec::new();
        let mut framework_bin: Option<PathBuf> = None;
        while let Some(a) = args_iter.next() {
            match a.as_str() {
                "--project-root" => project_root = args_iter.next().map(PathBuf::from),
                "--package-root" => {
                    package_roots.push(next_iter_value(&mut args_iter, "--package-root")?.into())
                }
                "--framework-bin" => framework_bin = args_iter.next().map(PathBuf::from),
                other => anyhow::bail!("unknown supervise argument: {}", other),
            }
        }
        let project_root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
        return Ok(CliCommand::Supervise {
            roots: PackageRoots::resolve(project_root, package_roots)?,
            framework_bin: framework_bin
                .ok_or_else(|| anyhow::anyhow!("missing --framework-bin"))?,
        });
    }
    if sub == "conformance" {
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::Conformance(parse_conformance_args(&rest)?));
    }
    if sub == "config" {
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::Config(parse_config_args(&rest)?));
    }
    if sub == "boundary-resources" {
        if let Some(other) = args_iter.next() {
            anyhow::bail!("unknown boundary-resources argument: {}", other);
        }
        return Ok(CliCommand::BoundaryResources);
    }
    if sub == "rate-acquire" {
        let pool = next_iter_value(&mut args_iter, "<pool>")?;
        if let Some(other) = args_iter.next() {
            anyhow::bail!("unknown rate-acquire argument: {}", other);
        }
        return Ok(CliCommand::RateAcquire { pool });
    }
    if sub == "test" {
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::Test(parse_test_args(&rest)?));
    }
    if sub == "init-package-repo" {
        let rest = args_iter.collect::<Vec<_>>();
        return Ok(CliCommand::InitPackageRepo(parse_init_package_repo_args(
            &rest,
        )?));
    }
    if sub == "run" {
        let lua_path: PathBuf = args_iter
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing <lua_file> arg"))?
            .into();
        let mut project_root: Option<PathBuf> = None;
        let mut package_roots: Vec<PathBuf> = Vec::new();
        let mut owner_namespace: Option<String> = None;
        let mut event_json: Option<String> = None;
        while let Some(a) = args_iter.next() {
            match a.as_str() {
                "--event" => event_json = args_iter.next(),
                "--project-root" => project_root = args_iter.next().map(PathBuf::from),
                "--package-root" => {
                    package_roots.push(next_iter_value(&mut args_iter, "--package-root")?.into());
                }
                "--owner-namespace" => {
                    owner_namespace = Some(next_iter_value(&mut args_iter, "--owner-namespace")?)
                }
                other => anyhow::bail!("unknown run argument: {}", other),
            }
        }
        let event_str = event_json.unwrap_or_else(|| "{}".into());
        let event: JsonValue = serde_json::from_str(&event_str)
            .with_context(|| format!("--event not valid json: {}", event_str))?;
        let project_root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
        let roots = PackageRoots::resolve_run(project_root, package_roots)?;
        // --owner-namespace is optional: a direct `run` with a single --package-root
        // (dogfood, package departments) defaults to that package's namespace. Multiple
        // package roots form the namespace catalog; the owner namespace still selects
        // the owner root whose require roots are installed into Lua package.path.
        let owner_namespace = match owner_namespace {
            Some(namespace) => namespace,
            None => roots
                .sole_package_namespace()
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "--owner-namespace is required when multiple --package-root are given"
                    )
                })?,
        };
        roots
            .name_resolver()
            .validate_owner(&owner_namespace)
            .with_context(|| format!("validate --owner-namespace `{owner_namespace}`"))?;
        return Ok(CliCommand::Run {
            lua_path,
            roots,
            owner_namespace,
            event,
        });
    }
    anyhow::bail!("unknown subcommand: {}", sub);
}

#[derive(Clone, Debug)]
struct ConfigCli {
    roots: PackageRoots,
}

#[derive(Clone, Debug)]
struct TestCli {
    roots: PackageRoots,
    report_json: Option<PathBuf>,
}

fn parse_conformance_args(args: &[String]) -> Result<HostConformanceOptions> {
    let mut project_root: Option<PathBuf> = None;
    let mut package_roots: Vec<PathBuf> = Vec::new();
    let mut report_json: Option<PathBuf> = None;
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
                i += 1;
                package_roots.push(next_value(args, i, "--package-root")?.into());
            }
            "--report-json" => {
                if report_json.is_some() {
                    anyhow::bail!("duplicate --report-json");
                }
                i += 1;
                report_json = Some(next_value(args, i, "--report-json")?.into());
            }
            other => anyhow::bail!("unknown conformance argument: {}", other),
        }
        i += 1;
    }

    let root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
    Ok(HostConformanceOptions {
        roots: PackageRoots::resolve(root, package_roots)?,
        report_json,
    })
}

fn parse_config_args(args: &[String]) -> Result<ConfigCli> {
    let mut project_root: Option<PathBuf> = None;
    let mut package_roots: Vec<PathBuf> = Vec::new();
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
                i += 1;
                package_roots.push(next_value(args, i, "--package-root")?.into());
            }
            other => anyhow::bail!("unknown config argument: {}", other),
        }
        i += 1;
    }

    let root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
    Ok(ConfigCli {
        roots: PackageRoots::resolve(root, package_roots)?,
    })
}

fn parse_test_args(args: &[String]) -> Result<TestCli> {
    let mut project_root: Option<PathBuf> = None;
    let mut package_roots: Vec<PathBuf> = Vec::new();
    let mut report_json: Option<PathBuf> = None;
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
                i += 1;
                package_roots.push(next_value(args, i, "--package-root")?.into());
            }
            "--report-json" => {
                if report_json.is_some() {
                    anyhow::bail!("duplicate --report-json");
                }
                i += 1;
                report_json = Some(next_value(args, i, "--report-json")?.into());
            }
            other => anyhow::bail!("unknown test argument: {}", other),
        }
        i += 1;
    }

    let root = project_root.ok_or_else(|| anyhow::anyhow!("missing --project-root"))?;
    Ok(TestCli {
        roots: PackageRoots::resolve(root, package_roots)?,
        report_json,
    })
}

fn parse_init_package_repo_args(
    args: &[String],
) -> Result<init_package_repo::InitPackageRepoOptions> {
    let mut substrate_ref: Option<String> = None;
    let mut force = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--ref" => {
                if substrate_ref.is_some() {
                    anyhow::bail!("duplicate --ref");
                }
                i += 1;
                substrate_ref = Some(next_value(args, i, "--ref")?);
            }
            "--force" => force = true,
            other => anyhow::bail!("unknown init-package-repo argument: {}", other),
        }
        i += 1;
    }

    let substrate_ref = match substrate_ref {
        Some(value) => value,
        None => {
            let pin = env!("FKST_FRAMEWORK_SOURCE_PIN").to_string();
            if pin == "unknown" {
                anyhow::bail!("missing --ref: build-time FKST_FRAMEWORK_SOURCE_PIN is unavailable");
            }
            pin
        }
    };
    Ok(init_package_repo::InitPackageRepoOptions {
        substrate_ref,
        force,
    })
}

fn next_iter_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing {} value", flag))
}

fn next_value(args: &[String], index: usize, flag: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing {} value", flag))
}

fn run_pipeline(
    lua_path: PathBuf,
    roots: PackageRoots,
    owner_namespace: String,
    event: JsonValue,
) -> Result<i32> {
    crate::process_tree::install_sdk_shutdown_watch();
    let parent_pid = std::env::var("FKST_SUPERVISOR_PID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    if parent_pid
        .map(crate::process_tree::parent_changed)
        .unwrap_or(false)
    {
        eprintln!("[framework] supervisor parent lost before pipeline start");
        return Ok(125);
    }
    let lua = mlua_init::new_lua();
    let raise_buf = RaiseBuffer::new();
    let owner_root = roots
        .owner_root_for_namespace(&owner_namespace)
        .ok_or_else(|| anyhow::anyhow!("unknown owner namespace `{owner_namespace}`"))?;
    let require_roots = roots.require_roots_for_owner(owner_root);
    let graph_json_authorized =
        sdk_graph::department_authorized(&roots, owner_root, &lua_path).unwrap_or(false);

    mlua_init::register_framework_sdk(
        &lua,
        raise_buf.clone(),
        roots.host_root(),
        owner_root,
        department_name_for_lua(&lua_path, owner_root, &owner_namespace),
        roots.name_resolver(),
        owner_namespace.clone(),
        Some(roots.clone()),
        graph_json_authorized,
    )?;

    let exit_code = match mlua_init::run_dept_with_require_roots(
        &lua,
        &lua_path,
        &event,
        require_roots.iter().map(|root| root.as_path()),
    ) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("[framework] pipeline failed: {:#}", e);
            1
        }
    };

    raise_buf.emit_stdout();
    Ok(exit_code)
}

fn department_name_for_lua(
    lua_path: &Path,
    owner_root: &Path,
    owner_namespace: &str,
) -> Option<String> {
    let rel = lua_path.strip_prefix(owner_root).ok()?;
    let mut components = rel.components();
    let first = components.next()?.as_os_str();
    let name = components.next()?.as_os_str().to_str()?;
    let last = components.next()?.as_os_str();
    if components.next().is_some()
        || first != std::ffi::OsStr::new("departments")
        || last != std::ffi::OsStr::new("main.lua")
    {
        return None;
    }
    if owner_namespace == crate::path_resolver::HOST_NAMESPACE {
        Some(name.to_string())
    } else {
        Some(format!("{owner_namespace}.{name}"))
    }
}

fn run_config_command(options: ConfigCli) -> Result<i32> {
    let config = config_registry::ConfigContext::from_host_root(options.roots.host_root())?;
    rate_pool::RatePoolRegistry::from_config(&config)?;
    for entry in config_registry::CONFIG_REGISTRY {
        let resolved = config.resolve(entry.key);
        let (value, source) = match resolved {
            Ok(resolved) => (resolved.value, resolved.source.label().to_string()),
            Err(_) => ("missing".to_string(), "missing".to_string()),
        };
        let requirement = match entry.kind.default() {
            Some(default) => format!("default={default}"),
            None => "required".to_string(),
        };
        println!(
            "name={} env={} kind={} type={} {} resolved={} source={} doc={}",
            entry.name,
            entry.env_key,
            entry.kind.label(),
            entry.value_type.label(),
            requirement,
            value,
            source,
            entry.doc
        );
    }
    Ok(0)
}

fn run_boundary_resources_command() -> Result<i32> {
    for entry in boundary_resource::BOUNDARY_RESOURCE_REGISTRY {
        println!(
            "id={} type={} adapters={} grant={} meter={} budget={} errors={}",
            entry.id,
            entry.resource_type,
            entry.adapters,
            entry.grant,
            entry.meter,
            entry.budget,
            entry.errors
        );
    }
    Ok(0)
}

fn run_rate_acquire(pool: &str) -> Result<i32> {
    let registry = rate_pool::RatePoolRegistry::from_env()
        .with_context(|| "parse rate pool configuration for rate-acquire")?;
    registry.acquire_for_name(pool)?;
    Ok(0)
}

fn run() -> Result<i32> {
    match parse_args()? {
        CliCommand::Run {
            lua_path,
            roots,
            owner_namespace,
            event,
        } => run_pipeline(lua_path, roots, owner_namespace, event),
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
        CliCommand::Conformance(options) => host_conformance::run(options),
        CliCommand::Config(options) => run_config_command(options),
        CliCommand::BoundaryResources => run_boundary_resources_command(),
        CliCommand::RateAcquire { pool } => run_rate_acquire(&pool),
        CliCommand::Test(options) => test_runner::run_tests(options.roots, options.report_json),
        CliCommand::InitPackageRepo(options) => init_package_repo::run(options),
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
