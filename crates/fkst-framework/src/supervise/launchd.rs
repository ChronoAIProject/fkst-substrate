use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LaunchdDeploymentUnit {
    label: String,
    framework_bin: PathBuf,
    project_root: PathBuf,
    package_roots: Vec<PathBuf>,
    runtime_root: PathBuf,
    durable_root: PathBuf,
}

impl LaunchdDeploymentUnit {
    pub(crate) fn new(
        label: String,
        framework_bin: PathBuf,
        project_root: PathBuf,
        package_roots: Vec<PathBuf>,
        runtime_root: PathBuf,
        durable_root: PathBuf,
    ) -> Result<Self> {
        validate_label(&label)?;
        validate_absolute_path("--framework-bin", &framework_bin)?;
        validate_absolute_path("--project-root", &project_root)?;
        validate_absolute_path("--runtime-root", &runtime_root)?;
        validate_absolute_path("--durable-root", &durable_root)?;
        for package_root in &package_roots {
            validate_absolute_path("--package-root", package_root)?;
        }
        Ok(Self {
            label,
            framework_bin,
            project_root,
            package_roots,
            runtime_root,
            durable_root,
        })
    }

    pub(crate) fn render_launchd_plist(&self) -> Result<String> {
        let arguments = self.supervise_arguments()?;
        let mut out = String::new();
        out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        out.push('\n');
        out.push_str(
            r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#,
        );
        out.push('\n');
        out.push_str("<plist version=\"1.0\">\n");
        out.push_str("<dict>\n");
        push_string_key(&mut out, "Label", &self.label);
        out.push_str("  <key>ProgramArguments</key>\n");
        out.push_str("  <array>\n");
        for argument in arguments {
            out.push_str("    <string>");
            out.push_str(&escape_xml(&argument));
            out.push_str("</string>\n");
        }
        out.push_str("  </array>\n");
        out.push_str("  <key>EnvironmentVariables</key>\n");
        out.push_str("  <dict>\n");
        push_string_key(
            &mut out,
            fkst_common::runtime_layout::RUNTIME_ROOT_ENV,
            path_to_utf8(&self.runtime_root)
                .with_context(|| format!("render {}", self.runtime_root.display()))?,
        );
        push_string_key(
            &mut out,
            fkst_common::DURABLE_ROOT_ENV,
            path_to_utf8(&self.durable_root)
                .with_context(|| format!("render {}", self.durable_root.display()))?,
        );
        push_string_key(
            &mut out,
            super::restart_authority::LAUNCHD_LABEL_ENV,
            &self.label,
        );
        out.push_str("  </dict>\n");
        out.push_str("  <key>KeepAlive</key>\n");
        out.push_str("  <true/>\n");
        out.push_str("  <key>AbandonProcessGroup</key>\n");
        out.push_str("  <true/>\n");
        out.push_str("</dict>\n");
        out.push_str("</plist>\n");
        Ok(out)
    }

    fn supervise_arguments(&self) -> Result<Vec<String>> {
        let framework_bin = path_to_utf8(&self.framework_bin)?.to_string();
        let mut args = vec![
            framework_bin.clone(),
            "supervise".to_string(),
            "--project-root".to_string(),
            path_to_utf8(&self.project_root)?.to_string(),
            "--framework-bin".to_string(),
            framework_bin,
        ];
        for package_root in &self.package_roots {
            args.push("--package-root".to_string());
            args.push(path_to_utf8(package_root)?.to_string());
        }
        Ok(args)
    }
}

#[derive(Default)]
pub(crate) struct RenderLaunchdArgs {
    label: Option<String>,
    framework_bin: Option<PathBuf>,
    project_root: Option<PathBuf>,
    package_roots: Vec<PathBuf>,
    runtime_root: Option<PathBuf>,
    durable_root: Option<PathBuf>,
}

pub(crate) fn parse_render_launchd_args(args: &[String]) -> Result<LaunchdDeploymentUnit> {
    let mut parsed = RenderLaunchdArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--label" => {
                set_once_string(&mut parsed.label, args, &mut i, "--label")?;
            }
            "--framework-bin" => {
                set_once_path(&mut parsed.framework_bin, args, &mut i, "--framework-bin")?;
            }
            "--project-root" => {
                set_once_path(&mut parsed.project_root, args, &mut i, "--project-root")?;
            }
            "--package-root" => {
                i += 1;
                parsed
                    .package_roots
                    .push(PathBuf::from(next_value(args, i, "--package-root")?));
            }
            "--runtime-root" => {
                set_once_path(&mut parsed.runtime_root, args, &mut i, "--runtime-root")?;
            }
            "--durable-root" => {
                set_once_path(&mut parsed.durable_root, args, &mut i, "--durable-root")?;
            }
            other => bail!("unknown supervise render-launchd argument: {other}"),
        }
        i += 1;
    }

    LaunchdDeploymentUnit::new(
        parsed.label.ok_or_else(|| anyhow!("missing --label"))?,
        parsed
            .framework_bin
            .ok_or_else(|| anyhow!("missing --framework-bin"))?,
        parsed
            .project_root
            .ok_or_else(|| anyhow!("missing --project-root"))?,
        parsed.package_roots,
        parsed
            .runtime_root
            .ok_or_else(|| anyhow!("missing --runtime-root"))?,
        parsed
            .durable_root
            .ok_or_else(|| anyhow!("missing --durable-root"))?,
    )
}

fn set_once_string(
    target: &mut Option<String>,
    args: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<()> {
    if target.is_some() {
        bail!("duplicate {flag}");
    }
    *index += 1;
    *target = Some(next_value(args, *index, flag)?);
    Ok(())
}

fn set_once_path(
    target: &mut Option<PathBuf>,
    args: &[String],
    index: &mut usize,
    flag: &str,
) -> Result<()> {
    let mut value = None;
    set_once_string(&mut value, args, index, flag)?;
    *target = value.map(PathBuf::from);
    Ok(())
}

fn next_value(args: &[String], index: usize, flag: &str) -> Result<String> {
    args.get(index)
        .cloned()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing {flag} value"))
}

fn validate_label(label: &str) -> Result<()> {
    if label.is_empty() {
        bail!("launchd label must not be empty");
    }
    if !label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        bail!("launchd label must contain only ASCII letters, digits, '.', '-', or '_'");
    }
    Ok(())
}

fn validate_absolute_path(flag: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{flag} must not be empty");
    }
    if !path.is_absolute() {
        bail!("{flag} must be absolute for launchd rendering");
    }
    path_to_utf8(path).map(|_| ())
}

fn path_to_utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))
}

fn push_string_key(out: &mut String, key: &str, value: &str) {
    out.push_str("  <key>");
    out.push_str(&escape_xml(key));
    out.push_str("</key>\n");
    out.push_str("  <string>");
    out.push_str(&escape_xml(value));
    out.push_str("</string>\n");
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_plist_escapes_xml_strings() {
        let unit = LaunchdDeploymentUnit::new(
            "com.example.fkst-test".to_string(),
            PathBuf::from("/opt/fkst/bin/fkst-framework"),
            PathBuf::from("/tmp/fkst<&>/host"),
            Vec::new(),
            PathBuf::from("/tmp/fkst/runtime"),
            PathBuf::from("/tmp/fkst/durable"),
        )
        .unwrap();

        let plist = unit.render_launchd_plist().unwrap();
        assert!(plist.contains("/tmp/fkst&lt;&amp;&gt;/host"), "{plist}");
    }

    #[test]
    fn launchd_plist_rejects_relative_command_paths() {
        let err = LaunchdDeploymentUnit::new(
            "com.example.fkst".to_string(),
            PathBuf::from("target/debug/fkst-framework"),
            PathBuf::from("/tmp/fkst/host"),
            Vec::new(),
            PathBuf::from("/tmp/fkst/runtime"),
            PathBuf::from("/tmp/fkst/durable"),
        )
        .unwrap_err();

        assert!(
            format!("{err:#}").contains("--framework-bin must be absolute"),
            "{err:#}"
        );
    }
}
