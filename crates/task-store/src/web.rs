//! Indexed Web queries and bounded, durable receipt and preview records.
use super::*;

impl TaskStore {
    /// Logical database bytes, including committed pages still held in the WAL.
    /// Callers reserve the uncheckpointed main-file growth before a write.
    pub fn logical_database_bytes(&self) -> Result<u64, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let pages: u64 = self
            .connection
            .query_row("PRAGMA page_count", [], |row| row.get(0))
            .map_err(|error| self.map_sqlite(error))?;
        let page_size: u64 = self
            .connection
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .map_err(|error| self.map_sqlite(error))?;
        pages
            .checked_mul(page_size)
            .ok_or_else(|| TaskStoreError::Limit("database size overflow".into()))
    }

    /// Maintain the rebuildable Web search projection from authenticated request metadata.
    pub fn index_web_task(
        &self,
        id: &TaskId,
        name: &str,
        batch: Option<&str>,
        workflow: &str,
    ) -> Result<(), TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if name.len() > 255 || batch.is_some_and(|v| v.len() > 32) || workflow.len() > 32 {
            return Err(TaskStoreError::Limit("Web metadata exceeds bounds".into()));
        }
        self.connection.execute("INSERT INTO web_tasks(task_id,name,batch,workflow) VALUES(?1,?2,?3,?4) ON CONFLICT(task_id) DO UPDATE SET name=excluded.name,batch=excluded.batch,workflow=excluded.workflow",
            params![id.as_str(), name, batch, workflow]).map_err(|error| self.map_sqlite(error))?;
        Ok(())
    }

    /// Store a bounded preview bound to its published artifact digest.
    pub fn write_web_preview(
        &self,
        id: &TaskId,
        key: &str,
        digest: &str,
        payload: &str,
    ) -> Result<(), TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if payload.len() > 2 * 1024 * 1024 {
            return Err(TaskStoreError::Limit("preview exceeds bounds".into()));
        }
        let transaction =
            self.connection.unchecked_transaction().map_err(|e| self.map_sqlite(e))?;
        transaction
            .execute(
                "DELETE FROM web_previews WHERE task_id=?1 AND storage_key=?2",
                params![id.as_str(), key],
            )
            .map_err(|e| self.map_sqlite(e))?;
        for (part, bytes) in payload.as_bytes().chunks(8192).enumerate() {
            transaction.execute("INSERT INTO web_previews(task_id,storage_key,source_sha,part,payload) VALUES(?1,?2,?3,?4,?5)", params![id.as_str(),key,digest,part as u32,bytes]).map_err(|e| self.map_sqlite(e))?;
        }
        transaction.commit().map_err(|e| self.map_sqlite(e))?;
        Ok(())
    }
    /// Fetch a preview only for the currently committed source digest.
    pub fn web_preview(
        &self,
        id: &TaskId,
        key: &str,
        digest: &str,
    ) -> Result<Option<String>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let mut statement = self.connection.prepare("SELECT payload FROM web_previews WHERE task_id=?1 AND storage_key=?2 AND source_sha=?3 ORDER BY part LIMIT 257").map_err(|e| self.map_sqlite(e))?;
        let rows = statement
            .query_map(params![id.as_str(), key, digest], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|e| self.map_sqlite(e))?;
        let mut bytes = Vec::new();
        for row in rows {
            bytes.extend(row.map_err(|e| self.map_sqlite(e))?);
            if bytes.len() > 2 * 1024 * 1024 {
                return Err(TaskStoreError::Limit("preview exceeds bounds".into()));
            }
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| TaskStoreError::Corrupt("invalid preview encoding".into()))
    }
    /// Inspect incomplete receipts during service recovery.
    pub fn pending_web_receipts(&self) -> Result<Vec<String>, TaskStoreError> {
        self.preflight()?;
        let mut statement = self.connection.prepare("SELECT id FROM web_receipts WHERE state IN ('waiting','uploading','receiving') LIMIT 100000").map_err(|e| self.map_sqlite(e))?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| self.map_sqlite(e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| self.map_sqlite(e))?;
        let mut result = Vec::new();
        for id in ids {
            if let Some(value) = self.web_receipt(&id)? {
                result.push(value);
            }
        }
        Ok(result)
    }

    /// Atomically create or compare-and-set a durable upload receipt.
    pub fn write_web_receipt(
        &self,
        id: &str,
        expected: Option<&str>,
        state: &str,
        task: Option<&TaskId>,
        payload: &str,
    ) -> Result<bool, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if id.len() != 32
            || !id.bytes().all(|b| b.is_ascii_hexdigit())
            || payload.len() > 65536
            || state.len() > 32
        {
            return Err(TaskStoreError::Limit("receipt exceeds bounds".into()));
        }
        let transaction =
            self.connection.unchecked_transaction().map_err(|e| self.map_sqlite(e))?;
        let count = if let Some(expected) = expected {
            transaction.execute(
                "UPDATE web_receipts SET state=?3,task_id=?4,updated_at_ms=?5 WHERE id=?1 AND state=?2",
                params![id, expected, state, task.map(TaskId::as_str), utc_now_ms()?],
            )
        } else {
            transaction.execute(
                "INSERT OR IGNORE INTO web_receipts(id,state,task_id,updated_at_ms) VALUES(?1,?2,?3,?4)",
                params![id, state, task.map(TaskId::as_str), utc_now_ms()?],
            )
        }
        .map_err(|e| self.map_sqlite(e))?;
        if count == 1 {
            transaction
                .execute("DELETE FROM web_receipt_parts WHERE receipt_id=?1", [id])
                .map_err(|e| self.map_sqlite(e))?;
            for (part, bytes) in payload.as_bytes().chunks(8192).enumerate() {
                transaction
                    .execute(
                        "INSERT INTO web_receipt_parts(receipt_id,part,payload) VALUES(?1,?2,?3)",
                        params![id, part as u32, bytes],
                    )
                    .map_err(|e| self.map_sqlite(e))?;
            }
        }
        transaction.commit().map_err(|e| self.map_sqlite(e))?;
        Ok(count == 1)
    }

    /// Reclaim one bounded page of abandoned receipts while preserving active transports.
    pub fn prune_web_receipts(&self, before_ms: i64) -> Result<usize, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        self.connection.execute(
            "DELETE FROM web_receipts WHERE id IN (SELECT id FROM web_receipts WHERE task_id IS NULL AND state IN ('waiting','failed','cancelled','interrupted') AND updated_at_ms<?1 LIMIT 1000)",
            [before_ms],
        ).map_err(|e| self.map_sqlite(e))
    }

    /// Read the bounded persisted upload receipt.
    pub fn web_receipt(&self, id: &str) -> Result<Option<String>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT payload FROM web_receipt_parts WHERE receipt_id=?1 ORDER BY part LIMIT 9",
            )
            .map_err(|e| self.map_sqlite(e))?;
        let rows = statement
            .query_map([id], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|e| self.map_sqlite(e))?;
        let mut bytes = Vec::new();
        for row in rows {
            bytes.extend(row.map_err(|e| self.map_sqlite(e))?);
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        if bytes.len() > 65536 {
            return Err(TaskStoreError::Limit("receipt exceeds bounds".into()));
        }
        String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| TaskStoreError::Corrupt("invalid receipt encoding".into()))
    }

    /// Query a bounded page through the Web metadata projection.
    pub fn list_web(
        &self,
        limit: u32,
        after: Option<&TaskCursor>,
        status: Option<TaskStatus>,
        pinned: Option<bool>,
        batch: Option<&str>,
        workflow: Option<&str>,
        search: &str,
        active: Option<bool>,
    ) -> Result<Vec<TaskRecord>, TaskStoreError> {
        let _operation = BusyOperation::enter(&self.busy)?;
        self.preflight()?;
        if limit == 0 || limit > 100 || search.len() > 255 {
            return Err(TaskStoreError::Limit("Web query exceeds bounds".into()));
        }
        let mut statement = self.connection.prepare("SELECT t.id FROM tasks t LEFT JOIN web_tasks w ON w.task_id=t.id WHERE (?1 IS NULL OR t.updated_at_ms<?1 OR (t.updated_at_ms=?1 AND t.id<?2)) AND (?3 IS NULL OR t.status=?3) AND (?4 IS NULL OR t.pinned=?4) AND (?5 IS NULL OR w.batch=?5) AND (?6 IS NULL OR w.workflow=?6) AND (?7='' OR instr(lower(w.name),lower(?7))>0) AND (?8 IS NULL OR (t.status IN ('pending','running','converted'))=?8) ORDER BY t.updated_at_ms DESC,t.id DESC LIMIT ?9").map_err(|e| self.map_sqlite(e))?;
        let rows = statement
            .query_map(
                params![
                    after.map(|c| c.updated_at_ms),
                    after.map(|c| c.id.as_str()),
                    status.map(TaskStatus::as_db),
                    pinned,
                    batch,
                    workflow,
                    search,
                    active,
                    limit
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| self.map_sqlite(e))?;
        let mut result = Vec::new();
        for row in rows {
            let id = TaskId::parse(row.map_err(|e| self.map_sqlite(e))?)?;
            if let Some(record) = self.get(&id)? {
                result.push(record);
            }
        }
        Ok(result)
    }
}
