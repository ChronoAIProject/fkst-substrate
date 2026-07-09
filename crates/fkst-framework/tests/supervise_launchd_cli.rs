use std::collections::BTreeMap;
use std::process::{Command, Output};

fn framework_bin() -> &'static str {
    env!("CARGO_BIN_EXE_fkst-framework")
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

#[test]
fn render_launchd_plist_carries_restart_contract_and_declared_command() {
    let temp = tempfile::Builder::new()
        .prefix("fkst launchd test")
        .tempdir()
        .unwrap();
    let root = temp.path();
    let rendered_framework_bin = root.join("bin/fkst-framework & renderer");
    let host = root.join("host");
    let package = root.join("package-alpha");
    let runtime = root.join("runtime");
    let durable = root.join("durable");

    let output = Command::new(framework_bin())
        .arg("supervise")
        .arg("render-launchd")
        .arg("--label")
        .arg("com.example.fkst-substrate.test")
        .arg("--framework-bin")
        .arg(&rendered_framework_bin)
        .arg("--project-root")
        .arg(&host)
        .arg("--package-root")
        .arg(&package)
        .arg("--runtime-root")
        .arg(&runtime)
        .arg("--durable-root")
        .arg(&durable)
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let plist = stdout(&output);
    let values = plist_values(&plist);

    assert_eq!(values.scalar("KeepAlive"), Some("true"), "plist: {plist}");
    assert_eq!(
        values.scalar("AbandonProcessGroup"),
        Some("true"),
        "plist: {plist}"
    );
    assert_eq!(
        values.scalar("FKST_RUNTIME_ROOT"),
        Some(runtime.to_str().unwrap()),
        "plist: {plist}"
    );
    assert_eq!(
        values.scalar("FKST_DURABLE_ROOT"),
        Some(durable.to_str().unwrap()),
        "plist: {plist}"
    );
    assert_eq!(
        values.scalar("FKST_LAUNCHD_LABEL"),
        Some("com.example.fkst-substrate.test"),
        "plist: {plist}"
    );
    assert_eq!(
        values.array("ProgramArguments"),
        Some(vec![
            rendered_framework_bin.to_str().unwrap().to_string(),
            "supervise".to_string(),
            "--project-root".to_string(),
            host.to_str().unwrap().to_string(),
            "--framework-bin".to_string(),
            rendered_framework_bin.to_str().unwrap().to_string(),
            "--package-root".to_string(),
            package.to_str().unwrap().to_string(),
        ]),
        "plist: {plist}"
    );
}

#[derive(Default)]
struct PlistValues {
    scalars: BTreeMap<String, String>,
    arrays: BTreeMap<String, Vec<String>>,
}

impl PlistValues {
    fn scalar(&self, key: &str) -> Option<&str> {
        self.scalars.get(key).map(String::as_str)
    }

    fn array(&self, key: &str) -> Option<Vec<String>> {
        self.arrays.get(key).cloned()
    }
}

fn plist_values(plist: &str) -> PlistValues {
    let tokens = plist_tokens(plist);
    let mut values = PlistValues::default();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] != "<key>" {
            i += 1;
            continue;
        }
        let key = xml_unescape(tokens.get(i + 1).expect("key body"));
        match tokens.get(i + 3).map(String::as_str) {
            Some("<true/>") => {
                values.scalars.insert(key, "true".to_string());
                i += 4;
            }
            Some("<false/>") => {
                values.scalars.insert(key, "false".to_string());
                i += 4;
            }
            Some("<string>") => {
                let value = xml_unescape(tokens.get(i + 4).expect("string body"));
                values.scalars.insert(key, value);
                i += 6;
            }
            Some("<array>") => {
                i += 4;
                let mut array = Vec::new();
                while tokens.get(i).map(String::as_str) != Some("</array>") {
                    assert_eq!(tokens.get(i).map(String::as_str), Some("<string>"));
                    array.push(xml_unescape(tokens.get(i + 1).expect("array string body")));
                    assert_eq!(tokens.get(i + 2).map(String::as_str), Some("</string>"));
                    i += 3;
                }
                values.arrays.insert(key, array);
                i += 1;
            }
            _ => i += 1,
        }
    }
    values
}

fn plist_tokens(plist: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut remaining = plist;
    while let Some(start) = remaining.find('<') {
        let text = remaining[..start].trim();
        if !text.is_empty() {
            tokens.push(text.to_string());
        }
        let after_start = &remaining[start..];
        let end = after_start.find('>').expect("xml token end");
        tokens.push(after_start[..=end].to_string());
        remaining = &after_start[end + 1..];
    }
    let text = remaining.trim();
    if !text.is_empty() {
        tokens.push(text.to_string());
    }
    tokens
}

fn xml_unescape(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&gt;", ">")
        .replace("&lt;", "<")
        .replace("&amp;", "&")
}
