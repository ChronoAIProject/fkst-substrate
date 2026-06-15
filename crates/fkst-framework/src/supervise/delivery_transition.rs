//! Durable delivery index transition helpers.

use super::delivery_index::{make_index_key, DueIndexKey, LEASED_BY_DEPT_UNTIL, READY_BY_DEPT_DUE};
use super::delivery_types::DeliveryRecord;
use anyhow::Result;
use redb::ReadableTable;
use std::collections::BTreeSet;

pub(crate) fn lease_key(
    key: DueIndexKey,
    now_ms: u64,
    lease_until: u64,
    dept: Option<&str>,
    excluded: &BTreeSet<String>,
    delivery: &mut redb::Table<'_, &str, &[u8]>,
    ready: &mut redb::Table<'_, &str, ()>,
    lease_index: &mut redb::Table<'_, &str, ()>,
    from_expired_lease: bool,
) -> Result<LeaseKeyOutcome> {
    let DueIndexKey {
        key: index_key,
        due_ms: indexed_at,
        delivery_id,
    } = key;
    if excluded.contains(&delivery_id) {
        return Ok(LeaseKeyOutcome::unchanged());
    }
    let Some(mut record) = read_delivery_table(delivery, &delivery_id)? else {
        if from_expired_lease {
            lease_index.remove(index_key.as_str())?;
        } else {
            ready.remove(index_key.as_str())?;
        }
        return Ok(LeaseKeyOutcome::mutated(None));
    };
    if dept.is_some_and(|wanted| record.dept != wanted) {
        return Ok(LeaseKeyOutcome::unchanged());
    }
    if from_expired_lease {
        lease_index.remove(index_key.as_str())?;
        if record.lease_until_ms != Some(indexed_at) {
            return Ok(LeaseKeyOutcome::mutated(None));
        }
    } else {
        ready.remove(index_key.as_str())?;
        if record.lease_until_ms.is_some() {
            return Ok(LeaseKeyOutcome::mutated(None));
        }
    }
    if record.not_before_ms > now_ms && !from_expired_lease {
        ready.insert(
            make_index_key(&record.dept, record.not_before_ms, delivery_id.as_str()).as_str(),
            &(),
        )?;
        return Ok(LeaseKeyOutcome::mutated(None));
    }
    if let Some(old_lease_until) = record.lease_until_ms {
        lease_index
            .remove(make_index_key(&record.dept, old_lease_until, delivery_id.as_str()).as_str())?;
    }
    record.lease_generation = record.lease_generation.saturating_add(1);
    record.lease_until_ms = Some(lease_until);
    let bytes = serde_json::to_vec(&record)?;
    delivery.insert(delivery_id.as_str(), bytes.as_slice())?;
    lease_index.insert(
        make_index_key(&record.dept, lease_until, delivery_id.as_str()).as_str(),
        &(),
    )?;
    Ok(LeaseKeyOutcome::mutated(Some(record)))
}

pub(crate) struct LeaseKeyOutcome {
    pub(crate) record: Option<DeliveryRecord>,
    pub(crate) mutated: bool,
}

impl LeaseKeyOutcome {
    fn unchanged() -> Self {
        Self {
            record: None,
            mutated: false,
        }
    }

    fn mutated(record: Option<DeliveryRecord>) -> Self {
        Self {
            record,
            mutated: true,
        }
    }
}

pub(crate) fn rebuild_due_indexes(
    write: &redb::WriteTransaction,
    delivery_by_id: redb::TableDefinition<&str, &[u8]>,
) -> Result<()> {
    write.delete_table(READY_BY_DEPT_DUE)?;
    write.delete_table(LEASED_BY_DEPT_UNTIL)?;
    let delivery = write.open_table(delivery_by_id)?;
    let mut ready = write.open_table(READY_BY_DEPT_DUE)?;
    let mut leased = write.open_table(LEASED_BY_DEPT_UNTIL)?;
    for entry in delivery.iter()? {
        let (_, bytes) = entry?;
        let record: DeliveryRecord = serde_json::from_slice(bytes.value())?;
        if let Some(lease_until) = record.lease_until_ms {
            leased.insert(
                make_index_key(&record.dept, lease_until, &record.delivery_id).as_str(),
                &(),
            )?;
        } else {
            ready.insert(
                make_index_key(&record.dept, record.not_before_ms, &record.delivery_id).as_str(),
                &(),
            )?;
        }
    }
    Ok(())
}

pub(crate) fn read_delivery_table(
    table: &redb::Table<'_, &str, &[u8]>,
    delivery_id: &str,
) -> Result<Option<DeliveryRecord>> {
    let Some(bytes) = table.get(delivery_id)? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(bytes.value())?))
}
