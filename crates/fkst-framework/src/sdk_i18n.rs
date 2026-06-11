//! SDK: key-based locale catalogs and `t(key[, vars])`.

use anyhow::{anyhow, bail, Context, Result};
use mlua::{Lua, Table, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

const OUTPUT_LANG_ENV: &str = "FKST_OUTPUT_LANG";
const REFERENCE_LOCALE: &str = "en";
const LOCALES_DIR: &str = "locales";
const FORBIDDEN_SOURCE_PATTERNS: &[&str] = &[
    "json.decode",
    "base64",
    "string.char",
    "utf8.char",
    "\\x",
    "\\u{",
];
const FORBIDDEN_CATALOG_TOKENS: &[&str] = &[
    "RAISED:",
    "SELF_TEST_FAILED:",
    "VERDICT:",
    "<!-- fkst:",
    "fkst:github-",
    "⟦AI:FKST⟧",
];

#[derive(Clone, Debug, Default)]
struct LocaleCatalogs {
    requested_locale: String,
    reference: BTreeMap<String, String>,
    selected: BTreeMap<String, String>,
}

pub(crate) fn register(lua: &Lua, owner_root: &Path) -> mlua::Result<()> {
    let catalogs = Arc::new(
        LocaleCatalogs::load(owner_root)
            .map_err(|err| mlua::Error::external(format!("load locale catalogs: {err:#}")))?,
    );
    lua.globals().set("t", {
        let catalogs = catalogs.clone();
        lua.create_function(move |_, (key, vars): (String, Option<Table>)| {
            catalogs.translate(&key, vars)
        })?
    })?;
    Ok(())
}

pub(crate) fn validate_graph_root_catalogs(root: &Path) -> Result<()> {
    let locales_dir = root.join(LOCALES_DIR);
    if !locales_dir.exists() {
        return Ok(());
    }
    if !locales_dir.is_dir() {
        bail!("{} must be a directory", locales_dir.display());
    }

    let catalogs = load_all_catalogs(&locales_dir)
        .with_context(|| format!("load locale catalogs from {}", locales_dir.display()))?;
    if catalogs.is_empty() {
        return Ok(());
    }
    let reference = catalogs
        .get(REFERENCE_LOCALE)
        .ok_or_else(|| anyhow!("locales/en.lua is required when locales/ is present"))?;
    for (locale, catalog) in &catalogs {
        if locale == REFERENCE_LOCALE {
            continue;
        }
        for key in reference.keys() {
            if !catalog.contains_key(key) {
                bail!("locale `{locale}` missing reference key `{key}`");
            }
        }
    }
    Ok(())
}

impl LocaleCatalogs {
    fn load(owner_root: &Path) -> Result<Self> {
        let requested_locale = requested_locale();
        let locales_dir = owner_root.join(LOCALES_DIR);
        if !locales_dir.exists() {
            return Ok(Self {
                requested_locale,
                reference: BTreeMap::new(),
                selected: BTreeMap::new(),
            });
        }

        let reference_path = locales_dir.join(format!("{REFERENCE_LOCALE}.lua"));
        let reference = if reference_path.is_file() {
            load_catalog_file(&reference_path)?
        } else {
            BTreeMap::new()
        };
        let selected_path = locales_dir.join(format!("{requested_locale}.lua"));
        let selected = if requested_locale == REFERENCE_LOCALE {
            reference.clone()
        } else if selected_path.is_file() {
            load_catalog_file(&selected_path)?
        } else {
            if !reference.is_empty() {
                crate::sdk_log::emit(
                    "warn",
                    &format!(
                        "i18n fallback=locale requested={} fallback={}",
                        requested_locale, REFERENCE_LOCALE
                    ),
                );
            }
            reference.clone()
        };

        Ok(Self {
            requested_locale,
            reference,
            selected,
        })
    }

    fn translate(&self, key: &str, vars: Option<Table>) -> mlua::Result<String> {
        validate_catalog_key(key)?;
        let template = match self.selected.get(key) {
            Some(value) => value.as_str(),
            None => match self.reference.get(key) {
                Some(value) => {
                    crate::sdk_log::emit(
                        "warn",
                        &format!(
                            "i18n fallback=key locale={} key={} fallback={}",
                            self.requested_locale, key, REFERENCE_LOCALE
                        ),
                    );
                    value.as_str()
                }
                None => {
                    return Err(mlua::Error::external(format!(
                        "t missing locale key `{key}`"
                    )));
                }
            },
        };

        interpolate(template, vars)
    }
}

fn requested_locale() -> String {
    std::env::var(OUTPUT_LANG_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .filter(|value| {
            if validate_locale_name(value).is_ok() {
                true
            } else {
                crate::sdk_log::emit(
                    "warn",
                    &format!(
                        "i18n fallback=invalid_locale requested={} fallback={}",
                        value, REFERENCE_LOCALE
                    ),
                );
                false
            }
        })
        .unwrap_or_else(|| REFERENCE_LOCALE.to_string())
}

fn load_all_catalogs(locales_dir: &Path) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut catalogs = BTreeMap::new();
    for entry in std::fs::read_dir(locales_dir)
        .with_context(|| format!("read {}", locales_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("lua") {
            continue;
        }
        let locale = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| anyhow!("locale file has no valid stem: {}", path.display()))?
            .to_string();
        validate_locale_name(&locale)?;
        let catalog = load_catalog_file(&path)?;
        catalogs.insert(locale, catalog);
    }
    Ok(catalogs)
}

fn load_catalog_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let source = std::fs::read_to_string(path)
        .with_context(|| format!("read locale catalog {}", path.display()))?;
    reject_forbidden_source_patterns(path, &source)?;

    let lua = Lua::new();
    let value = lua
        .load(&source)
        .set_name(path.display().to_string())
        .eval::<Value>()
        .with_context(|| format!("eval locale catalog {}", path.display()))?;
    let Value::Table(table) = value else {
        bail!("locale catalog {} must return a table", path.display());
    };

    let mut catalog = BTreeMap::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let key = match key {
            Value::String(value) => value
                .to_str()
                .map_err(|_| anyhow!("locale catalog key must be valid UTF-8"))?
                .to_string(),
            other => bail!("locale catalog key must be string, got {}", other.type_name()),
        };
        validate_catalog_key(&key)?;
        let value = match value {
            Value::String(value) => value
                .to_str()
                .map_err(|_| anyhow!("locale catalog value for `{key}` must be valid UTF-8"))?
                .to_string(),
            other => bail!(
                "locale catalog value for `{}` must be string, got {}",
                key,
                other.type_name()
            ),
        };
        reject_machine_tokens("locale catalog key", &key)?;
        reject_machine_tokens("locale catalog value", &value)?;
        catalog.insert(key, value);
    }
    Ok(catalog)
}

fn reject_forbidden_source_patterns(path: &Path, source: &str) -> Result<()> {
    for pattern in FORBIDDEN_SOURCE_PATTERNS {
        if source.contains(pattern) {
            bail!(
                "locale catalog {} contains forbidden decode helper pattern `{}`",
                path.display(),
                pattern
            );
        }
    }
    Ok(())
}

fn reject_machine_tokens(label: &str, value: &str) -> Result<()> {
    for token in FORBIDDEN_CATALOG_TOKENS {
        if value.contains(token) {
            bail!("{label} contains forbidden machine token `{token}`");
        }
    }
    Ok(())
}

fn validate_locale_name(locale: &str) -> Result<()> {
    if locale.is_empty() {
        bail!("locale name must not be empty");
    }
    if !locale
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        bail!("locale name `{locale}` must match [A-Za-z0-9_-]+");
    }
    Ok(())
}

fn validate_catalog_key(key: &str) -> mlua::Result<()> {
    if key.is_empty() {
        return Err(mlua::Error::external("locale catalog key must not be empty"));
    }
    if !key.bytes().all(|byte| {
        matches!(
            byte,
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.'
        )
    }) {
        return Err(mlua::Error::external(format!(
            "locale catalog key `{key}` must match [A-Za-z0-9_.-]+"
        )));
    }
    Ok(())
}

fn interpolate(template: &str, vars: Option<Table>) -> mlua::Result<String> {
    let vars = match vars {
        Some(vars) => vars_to_strings(vars)?,
        None => BTreeMap::new(),
    };
    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        output.push_str(&rest[..open]);
        let after_open = &rest[open + 1..];
        let close = after_open.find('}').ok_or_else(|| {
            mlua::Error::external("t interpolation placeholder is missing closing `}`")
        })?;
        let name = &after_open[..close];
        validate_placeholder_name(name)?;
        let value = vars.get(name).ok_or_else(|| {
            mlua::Error::external(format!("t missing interpolation variable `{name}`"))
        })?;
        output.push_str(value);
        rest = &after_open[close + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn vars_to_strings(vars: Table) -> mlua::Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for pair in vars.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let key = match key {
            Value::String(value) => value.to_str()?.to_string(),
            other => {
                return Err(mlua::Error::external(format!(
                    "t interpolation variable name must be string, got {}",
                    other.type_name()
                )))
            }
        };
        validate_placeholder_name(&key)?;
        let value = match value {
            Value::String(value) => value.to_str()?.to_string(),
            Value::Integer(value) => value.to_string(),
            Value::Number(value) => value.to_string(),
            Value::Boolean(value) => value.to_string(),
            other => {
                return Err(mlua::Error::external(format!(
                    "t interpolation variable `{}` must be scalar, got {}",
                    key,
                    other.type_name()
                )))
            }
        };
        out.insert(key, value);
    }
    Ok(out)
}

fn validate_placeholder_name(name: &str) -> mlua::Result<()> {
    if name.is_empty() {
        return Err(mlua::Error::external(
            "t interpolation variable name must not be empty",
        ));
    }
    if !name
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
    {
        return Err(mlua::Error::external(format!(
            "t interpolation variable `{name}` must match [A-Za-z0-9_]+"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use tempfile::TempDir;

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        old: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_output_lang(value: &str) -> Self {
            let lock = ENV_LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            let old = std::env::var_os(OUTPUT_LANG_ENV);
            std::env::set_var(OUTPUT_LANG_ENV, value);
            Self { _lock: lock, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => std::env::set_var(OUTPUT_LANG_ENV, value),
                None => std::env::remove_var(OUTPUT_LANG_ENV),
            }
        }
    }

    fn write_catalog(root: &Path, locale: &str, body: &str) {
        std::fs::create_dir_all(root.join("locales")).unwrap();
        std::fs::write(root.join("locales").join(format!("{locale}.lua")), body).unwrap();
    }

    #[test]
    fn t_resolves_requested_locale_and_interpolates_scalars() {
        let dir = TempDir::new().unwrap();
        write_catalog(
            dir.path(),
            "en",
            r#"return { ["hello.title"] = "Hello {name} #{count}" }"#,
        );
        write_catalog(
            dir.path(),
            "zh-CN",
            r#"return { ["hello.title"] = "你好 {name} #{count}" }"#,
        );
        let _env = EnvGuard::set_output_lang("zh-CN");
        let lua = Lua::new();
        register(&lua, dir.path()).unwrap();

        let value: String = lua
            .load(r#"return t("hello.title", { name = "Kai", count = 7 })"#)
            .eval()
            .unwrap();

        assert_eq!(value, "你好 Kai #7");
    }

    #[test]
    fn t_falls_back_to_en_when_key_missing_in_selected_locale() {
        let dir = TempDir::new().unwrap();
        write_catalog(dir.path(), "en", r#"return { answer = "Answer" }"#);
        write_catalog(dir.path(), "zh", r#"return { other = "其它" }"#);
        let _env = EnvGuard::set_output_lang("zh");
        let lua = Lua::new();
        register(&lua, dir.path()).unwrap();

        let value: String = lua.load(r#"return t("answer")"#).eval().unwrap();

        assert_eq!(value, "Answer");
    }

    #[test]
    fn t_rejects_output_lang_path_traversal_and_falls_back_to_en() {
        let dir = TempDir::new().unwrap();
        write_catalog(dir.path(), "en", r#"return { answer = "Answer" }"#);
        std::fs::write(dir.path().join("core.lua"), r#"return { answer = "Pwned" }"#).unwrap();
        let _env = EnvGuard::set_output_lang("../core");
        let lua = Lua::new();
        register(&lua, dir.path()).unwrap();

        let value: String = lua.load(r#"return t("answer")"#).eval().unwrap();

        assert_eq!(value, "Answer");
    }

    #[test]
    fn conformance_rejects_incomplete_non_reference_locale() {
        let dir = TempDir::new().unwrap();
        write_catalog(dir.path(), "en", r#"return { a = "A", b = "B" }"#);
        write_catalog(dir.path(), "zh", r#"return { a = "甲" }"#);

        let err = validate_graph_root_catalogs(dir.path()).unwrap_err();

        assert!(format!("{err:#}").contains("missing reference key `b`"));
    }

    #[test]
    fn conformance_rejects_decode_helper_patterns() {
        let dir = TempDir::new().unwrap();
        write_catalog(dir.path(), "en", r#"return { a = string.char(65) }"#);

        let err = validate_graph_root_catalogs(dir.path()).unwrap_err();

        assert!(format!("{err:#}").contains("forbidden decode helper pattern"));
    }

    #[test]
    fn conformance_rejects_machine_tokens_in_values() {
        let dir = TempDir::new().unwrap();
        write_catalog(dir.path(), "en", r#"return { a = "RAISED: value" }"#);

        let err = validate_graph_root_catalogs(dir.path()).unwrap_err();

        assert!(format!("{err:#}").contains("forbidden machine token"));
    }
}
