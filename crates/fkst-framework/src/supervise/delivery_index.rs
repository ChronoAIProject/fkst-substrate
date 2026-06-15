//! Department-scoped durable delivery secondary indexes.

use anyhow::{anyhow, Result};
use redb::{ReadableTable, TableDefinition};

const MILLIS_WIDTH: usize = 20;

pub(crate) const READY_BY_DEPT_DUE: TableDefinition<&str, ()> =
    TableDefinition::new("ready_by_dept_due");
pub(crate) const LEASED_BY_DEPT_UNTIL: TableDefinition<&str, ()> =
    TableDefinition::new("leased_by_dept_until");
pub(crate) const DEAD_BY_DEPT_DUE: TableDefinition<&str, ()> =
    TableDefinition::new("dead_by_dept_due");

pub(crate) struct DueIndexKey {
    pub(crate) key: String,
    pub(crate) due_ms: u64,
    pub(crate) delivery_id: String,
}

pub(crate) fn make_index_key(dept: &str, due_ms: u64, delivery_id: &str) -> String {
    format!(
        "{}/{due_ms:0MILLIS_WIDTH$}/{}",
        encode_index_part(dept),
        delivery_id
    )
}

pub(crate) fn make_dead_due_index_key(dept: &str, dead_at_ms: u64, delivery_id: &str) -> String {
    format!(
        "{dead_at_ms:0MILLIS_WIDTH$}/{}/{}",
        encode_index_part(dept),
        delivery_id
    )
}

pub(crate) fn collect_due_keys<T>(
    table: &T,
    dept: Option<&str>,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<DueIndexKey>>
where
    T: ReadableTable<&'static str, ()>,
{
    if limit == 0 {
        return Ok(Vec::new());
    }
    match dept {
        Some(dept) => collect_due_keys_for_dept(table, dept, now_ms, limit),
        None => collect_due_keys_for_all_depts(table, now_ms, limit),
    }
}

#[cfg(test)]
pub(crate) fn count_index_entries(table: &redb::ReadOnlyTable<&str, ()>) -> Result<usize> {
    let mut count = 0;
    for entry in table.iter()? {
        let _ = entry?;
        count += 1;
    }
    Ok(count)
}

fn collect_due_keys_for_dept<T>(
    table: &T,
    dept: &str,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<DueIndexKey>>
where
    T: ReadableTable<&'static str, ()>,
{
    let dept_prefix = format!("{}/", encode_index_part(dept));
    let start = format!("{dept_prefix}{:0MILLIS_WIDTH$}/", 0);
    let end = format!("{dept_prefix}{now_ms:0MILLIS_WIDTH$}/\u{10ffff}");
    collect_due_keys_in_range(table, start.as_str()..=end.as_str(), limit)
}

fn collect_due_keys_for_all_depts<T>(
    table: &T,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<DueIndexKey>>
where
    T: ReadableTable<&'static str, ()>,
{
    let mut keys = Vec::new();
    for entry in table.iter()? {
        let (key, _) = entry?;
        let key = key.value().to_string();
        let parsed = parse_index_key(&key)?;
        if parsed.due_ms <= now_ms {
            keys.push(parsed);
            if keys.len() >= limit {
                break;
            }
        }
    }
    Ok(keys)
}

fn collect_due_keys_in_range<T>(
    table: &T,
    range: std::ops::RangeInclusive<&str>,
    limit: usize,
) -> Result<Vec<DueIndexKey>>
where
    T: ReadableTable<&'static str, ()>,
{
    let mut keys = Vec::new();
    for entry in table.range::<&str>(range)? {
        let (key, _) = entry?;
        keys.push(parse_index_key(key.value())?);
        if keys.len() >= limit {
            break;
        }
    }
    Ok(keys)
}

fn parse_index_key(key: &str) -> Result<DueIndexKey> {
    let mut parts = key.splitn(3, '/');
    let _dept = parts
        .next()
        .ok_or_else(|| anyhow!("delivery index key missing dept"))?;
    let due = parts
        .next()
        .ok_or_else(|| anyhow!("delivery index key missing due time"))?;
    let delivery_id = parts
        .next()
        .ok_or_else(|| anyhow!("delivery index key missing delivery id"))?;
    let due_ms = due.parse::<u64>()?;
    Ok(DueIndexKey {
        key: key.to_string(),
        due_ms,
        delivery_id: delivery_id.to_string(),
    })
}

pub(crate) fn parse_dead_due_index_key(key: &str) -> Result<DueIndexKey> {
    let mut parts = key.splitn(3, '/');
    let due = parts
        .next()
        .ok_or_else(|| anyhow!("dead delivery index key missing due time"))?;
    let _dept = parts
        .next()
        .ok_or_else(|| anyhow!("dead delivery index key missing dept"))?;
    let delivery_id = parts
        .next()
        .ok_or_else(|| anyhow!("dead delivery index key missing delivery id"))?;
    let due_ms = due.parse::<u64>()?;
    Ok(DueIndexKey {
        key: key.to_string(),
        due_ms,
        delivery_id: delivery_id.to_string(),
    })
}

fn encode_index_part(part: &str) -> String {
    if part.is_empty() {
        return "_".to_string();
    }
    let mut encoded = String::new();
    for byte in part.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' => encoded.push(byte as char),
            b'_' => encoded.push_str("__"),
            _ => encoded.push_str(&format!("_x{byte:02X}")),
        }
    }
    if encoded.bytes().all(|byte| byte == b'.') {
        format!("_x{}", encoded)
    } else {
        encoded
    }
}
