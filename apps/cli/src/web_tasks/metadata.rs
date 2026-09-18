//! Database/WAL growth reservations independent of concurrently written task files.
use super::*;

struct DatabaseBudget {
    directory: SafeDir,
    main: File,
    bytes: u64,
    main_bytes: u64,
    checkpoint_debt: u64,
    physical_reservation: u64,
}
fn database_budget(
    shared: &Shared,
    store: &TaskStore,
    reservation: u64,
) -> Result<DatabaseBudget, WebTaskError> {
    let directory = shared
        .root_handle
        .open_child_private(std::ffi::OsStr::new("database"))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let main = directory
        .open_regular_private(std::ffi::OsStr::new("tasks.sqlite3"))
        .map_err(|error| WebTaskError::Unsafe(error.to_string()))?;
    let main_bytes = main.metadata()?.len();
    let checkpoint_debt = store.logical_database_bytes()?.saturating_sub(main_bytes);
    // New pages can occupy the WAL and main database simultaneously. Already
    // committed WAL pages may also grow the main file at the next checkpoint.
    let physical_reservation = reservation
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(checkpoint_debt))
        .ok_or_else(|| WebTaskError::Limit("database reservation overflow".into()))?;
    let bytes = measured_managed_bytes(&directory)?;
    Ok(DatabaseBudget { directory, main, bytes, main_bytes, checkpoint_debt, physical_reservation })
}

pub(super) fn metadata_store_mutation<T>(
    shared: &Shared,
    reservation: u64,
    mutation: impl FnOnce(&mut TaskStore) -> Result<T, WebTaskError>,
) -> Result<T, WebTaskError> {
    let mut quota = lock(&shared.disk_bytes);
    quota.revision = quota.revision.wrapping_add(1);
    // All measurements and mutations share the SQLite owner. File transfer and
    // publication remain outside this lock and use their own quota reservations.
    let mut store = lock(&shared.task_store);
    let preflight = (|| {
        let database = database_budget(shared, &store, reservation)?;
        let planned = quota
            .used
            .checked_add(quota.reserved)
            .and_then(|bytes| bytes.checked_add(database.physical_reservation));
        if planned.is_none_or(|total| total > MAX_GLOBAL_BYTES) {
            return Err(WebTaskError::Limit(
                "durable task metadata reservation is unavailable".into(),
            ));
        }
        Ok(database)
    })();
    let database = match preflight {
        Ok(value) => value,
        Err(error) => {
            drop(store);
            drop(quota);
            stop_unhealthy(shared);
            return Err(error);
        }
    };
    let result = mutation(&mut store);
    let settlement = (|| {
        let after = measured_managed_bytes(&database.directory)?;
        let main_growth = database.main.metadata()?.len().saturating_sub(database.main_bytes);
        Ok::<_, WebTaskError>((after, main_growth))
    })();
    drop(store);
    let settlement = settlement.and_then(|(after, main_growth)| {
        let growth = after.saturating_sub(database.bytes);
        quota.used = quota.used.saturating_add(growth);
        // Shrinking the database is settled by the background storage audit.
        // Admission uses the conservatively charged count until that audit.
        // Retain the per-mutation journal bound as well as the complete physical
        // budget, including checkpoint debt. Unrelated file writes never enter it.
        if growth > database.physical_reservation || quota.used.checked_add(quota.reserved).is_none_or(|bytes| bytes > MAX_GLOBAL_BYTES)
            || growth.saturating_sub(main_growth) > reservation
            || main_growth > database.checkpoint_debt.saturating_add(reservation) {
            eprintln!("web stage=storageCheck code=metadataReservationExceeded growth={growth} main_growth={main_growth} checkpoint_debt={} reservation={reservation}", database.checkpoint_debt);
            return Err(WebTaskError::Unsafe("durable task metadata exceeded its physical reservation".into()));
        }
        Ok(())
    });
    shared.disk_changed.notify_all();
    if let Err(error) = settlement {
        quota.used = quota.used.saturating_add(database.physical_reservation).min(MAX_GLOBAL_BYTES);
        drop(quota);
        stop_unhealthy(shared);
        return Err(error);
    }
    result
}

pub(super) fn mark_converted(shared: &Shared, id: &TaskId) -> Result<(), WebTaskError> {
    let current = lock(&shared.task_store).get(id)?.ok_or(WebTaskError::NotFound)?;
    if current.status == TaskStatus::Running {
        let converted_record =
            metadata_store_mutation(shared, STORE_MUTATION_RESERVATION, |store| {
                Ok(store.transition(
                    id,
                    TaskTransition {
                        expected: TaskStatus::Running,
                        next: TaskStatus::Converted,
                        progress_millionths: 900_000,
                        diagnostics: Vec::new(),
                        artifacts: Vec::new(),
                    },
                )?)
            })?;
        shared.events.publish_snapshot(&converted_record);
    }
    Ok(())
}
