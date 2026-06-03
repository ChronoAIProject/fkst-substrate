//! SDK: `log.info(msg)`, `log.warn(msg)`, `log.error(msg)` — all write structured lines to stderr.

use mlua::{Lua, Result};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn register(lua: &Lua) -> Result<()> {
    let log = lua.create_table()?;
    log.set(
        "info",
        lua.create_function(|_, msg: String| {
            eprintln!("{}", log_line("info", &msg));
            Ok(())
        })?,
    )?;
    log.set(
        "warn",
        lua.create_function(|_, msg: String| {
            eprintln!("{}", log_line("warn", &msg));
            Ok(())
        })?,
    )?;
    log.set(
        "error",
        lua.create_function(|_, msg: String| {
            eprintln!("{}", log_line("error", &msg));
            Ok(())
        })?,
    )?;
    lua.globals().set("log", log)?;
    Ok(())
}

fn log_line(level: &str, msg: &str) -> String {
    format!(
        "TIMESTAMP={} LEVEL={} MSG={}",
        rfc3339_now(),
        level,
        single_line(msg)
    )
}

fn single_line(msg: &str) -> String {
    msg.replace('\r', "\\r").replace('\n', "\\n")
}

fn rfc3339_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    rfc3339_from_unix(elapsed.as_secs())
}

fn rfc3339_from_unix(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let seconds_of_day = secs % 86_400;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn log_table_exists_after_register() {
        let lua = Lua::new();
        register(&lua).unwrap();
        let log: mlua::Table = lua.globals().get("log").unwrap();
        assert!(log.get::<mlua::Function>("info").is_ok());
        assert!(log.get::<mlua::Function>("warn").is_ok());
        assert!(log.get::<mlua::Function>("error").is_ok());
    }

    #[test]
    fn log_info_callable_from_lua() {
        let lua = Lua::new();
        register(&lua).unwrap();
        // Just verify no error on call.
        lua.load(r#"log.info("test message")"#).exec().unwrap();
    }

    #[test]
    fn log_line_has_timestamp_level_and_message_fields() {
        let line = log_line("info", "test message");
        assert!(line.starts_with("TIMESTAMP="));
        assert!(line.contains("T"));
        assert!(line.contains("Z LEVEL=info MSG=test message"));
    }

    #[test]
    fn timestamp_is_rfc3339_utc_seconds() {
        assert_eq!(rfc3339_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_from_unix(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
