use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use super::types::{
    ApprovalCheckpoint, AttemptLease, ChildRunPlan, EventPage, ExecutionCursor,
    ReconciliationSummary, ResumeActions, ResumeReceipt, RunListEntry, RunSnapshot,
    TERMINAL_RUN_STATES, TaskSummary, ToolCheckpoint, ToolOutcome, TranscriptProjection,
};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ExecutionStoreError {
    #[error("execution store unavailable: {0}")]
    Storage(#[from] rusqlite::Error),
    #[error("run not found: {0}")]
    NotFound(String),
    #[error("version conflict: expected {expected}, current {current}")]
    VersionConflict { expected: i64, current: i64 },
    #[error("idempotency conflict for request {request_id}")]
    IdempotencyConflict { request_id: String },
    #[error("run {run_id} is terminal: {state}")]
    AlreadyTerminal { run_id: String, state: String },
    #[error("run {run_id} already has active resume operation {operation_id}")]
    AlreadyInProgress {
        run_id: String,
        operation_id: String,
    },
    #[error("invalid execution cursor: storeId={store_id}, seq={seq}")]
    InvalidCursor { store_id: String, seq: i64 },
    #[error("execution store invariant violated: {0}")]
    Invariant(String),
}

pub struct ExecutionStore {
    path: PathBuf,
    conn: Connection,
    store_id: String,
}

pub type StoreResult<T> = Result<T, ExecutionStoreError>;

impl ExecutionStore {
    pub fn open_default() -> StoreResult<Self> {
        Self::open(crate::paths::root().join("execution.sqlite3"))
    }

    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        }
        let conn = Connection::open(path.as_ref())?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
        let proposed_store_id = format!("exec-{}", uuid::Uuid::new_v4());
        Self::initialize(conn, &proposed_store_id).and_then(|conn| {
            conn.query_row(
                "SELECT store_id FROM store_meta WHERE singleton=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|store_id| Self {
                path: path.as_ref().to_path_buf(),
                conn,
                store_id,
            })
            .map_err(Into::into)
        })
    }

    fn initialize(conn: Connection, store_id: &str) -> StoreResult<Connection> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS store_meta (
              singleton INTEGER PRIMARY KEY CHECK(singleton=1),
              store_id TEXT NOT NULL,
              schema_version INTEGER NOT NULL,
              host_epoch INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
              run_id TEXT PRIMARY KEY,
              session_id TEXT NOT NULL,
              branch_id TEXT NOT NULL,
              state TEXT NOT NULL CHECK(state IN ('queued','running','recovering','waiting_children','waiting_approval','paused','blocked','succeeded','failed','cancelled')),
              reason_code TEXT,
              version INTEGER NOT NULL CHECK(version >= 0),
              generation INTEGER NOT NULL CHECK(generation >= 0),
              next_event_seq INTEGER NOT NULL CHECK(next_event_seq >= 0),
              recovery_plan_id TEXT,
              desired_state TEXT NOT NULL,
              auto_resume INTEGER NOT NULL CHECK(auto_resume IN (0,1)),
              input_json TEXT NOT NULL,
              env_json TEXT NOT NULL,
              created_ms INTEGER NOT NULL,
              updated_ms INTEGER NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS one_live_run_per_branch
              ON runs(session_id, branch_id) WHERE state NOT IN ('succeeded','failed','cancelled');
            CREATE TABLE IF NOT EXISTS tasks (
              task_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              parent_task_id TEXT REFERENCES tasks(task_id),
              logical_key TEXT NOT NULL,
              kind TEXT NOT NULL,
              state TEXT NOT NULL CHECK(state IN ('pending','running','waiting','succeeded','failed','cancelled','uncertain')),
              version INTEGER NOT NULL CHECK(version >= 0),
              checkpoint_version INTEGER NOT NULL CHECK(checkpoint_version >= 0),
              checkpoint_json TEXT NOT NULL,
              result_json TEXT,
              policy_json TEXT NOT NULL,
              UNIQUE(run_id, logical_key)
            );
            CREATE TABLE IF NOT EXISTS attempts (
              attempt_id TEXT PRIMARY KEY,
              task_id TEXT NOT NULL REFERENCES tasks(task_id),
              attempt_no INTEGER NOT NULL CHECK(attempt_no >= 0),
              host_epoch INTEGER NOT NULL CHECK(host_epoch >= 0),
              generation INTEGER NOT NULL CHECK(generation >= 0),
              fence INTEGER NOT NULL CHECK(fence >= 0),
              worker_id TEXT,
              phase TEXT NOT NULL CHECK(phase IN ('claimed','executing','succeeded','failed','cancelled','uncertain')),
              started_ms INTEGER,
              lease_deadline_ms INTEGER,
              outcome_json TEXT,
              UNIQUE(task_id, attempt_no)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_attempt
              ON attempts(task_id) WHERE phase IN ('claimed','executing');
            CREATE TABLE IF NOT EXISTS resume_ops (
              operation_id TEXT PRIMARY KEY,
              principal_id TEXT NOT NULL,
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              request_id TEXT NOT NULL,
              request_hash TEXT NOT NULL,
              state TEXT NOT NULL CHECK(state IN ('accepted','reconciling','dispatching','completed','failed')),
              accepted_json TEXT NOT NULL,
              accepted_version INTEGER NOT NULL CHECK(accepted_version >= 0),
              UNIQUE(principal_id, run_id, request_id)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS one_active_resume
              ON resume_ops(run_id) WHERE state IN ('accepted','reconciling','dispatching');
            CREATE TABLE IF NOT EXISTS run_events (
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              seq INTEGER NOT NULL CHECK(seq >= 0),
              run_version INTEGER NOT NULL,
              kind TEXT NOT NULL,
              payload_json TEXT NOT NULL,
              PRIMARY KEY(run_id, seq)
            );
            CREATE TABLE IF NOT EXISTS outbox (
              outbox_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              effect_key TEXT NOT NULL UNIQUE,
              kind TEXT NOT NULL,
              payload_json TEXT NOT NULL,
              delivered_ms INTEGER
            );
            CREATE TABLE IF NOT EXISTS transcript_entries (
              entry_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              ordinal INTEGER NOT NULL,
              payload_json TEXT NOT NULL,
              UNIQUE(run_id, ordinal)
            );
            CREATE TABLE IF NOT EXISTS task_edges (
              parent_id TEXT NOT NULL REFERENCES tasks(task_id),
              child_id TEXT NOT NULL REFERENCES tasks(task_id),
              edge_key TEXT NOT NULL,
              ownership TEXT NOT NULL CHECK(ownership IN ('owned','detached')),
              wait_policy TEXT NOT NULL CHECK(wait_policy IN ('all_settled','first_success')),
              PRIMARY KEY(parent_id, edge_key),
              UNIQUE(parent_id, child_id)
            );
            CREATE TABLE IF NOT EXISTS queued_inputs (
              input_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              payload_json TEXT NOT NULL,
              consumed_by_task TEXT REFERENCES tasks(task_id)
            );
            CREATE TABLE IF NOT EXISTS approvals (
              approval_id TEXT PRIMARY KEY,
              run_id TEXT NOT NULL REFERENCES runs(run_id),
              task_id TEXT NOT NULL REFERENCES tasks(task_id),
              action_digest TEXT NOT NULL,
              generation INTEGER NOT NULL CHECK(generation >= 0),
              policy_version TEXT NOT NULL,
              status TEXT NOT NULL CHECK(status IN ('pending','approved','rejected','consumed','stale')),
              expires_ms INTEGER,
              decision_json TEXT,
              version INTEGER NOT NULL CHECK(version >= 0)
            );
            "#,
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO store_meta(singleton, store_id, schema_version, host_epoch) VALUES(1, ?1, ?2, 0)",
            params![store_id, SCHEMA_VERSION],
        )?;
        conn.query_row(
            "SELECT store_id FROM store_meta WHERE singleton=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(Some)
        .or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
        Ok(conn)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    pub fn reconcile_on_open(&mut self) -> StoreResult<ReconciliationSummary> {
        self.transaction_mut(|store| {
            let (schema_version, old_epoch): (i64, i64) = store.conn.query_row(
                "SELECT schema_version,host_epoch FROM store_meta WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if schema_version != SCHEMA_VERSION {
                return Err(ExecutionStoreError::Invariant(format!(
                    "unsupported schema {schema_version}, expected {SCHEMA_VERSION}"
                )));
            }
            let epoch = old_epoch.saturating_add(1);
            store.conn.execute(
                "UPDATE store_meta SET host_epoch=?1 WHERE singleton=1",
                params![epoch],
            )?;
            let mut stmt = store.conn.prepare(
                "SELECT run_id FROM runs WHERE state NOT IN ('succeeded','failed','cancelled')",
            )?;
            let run_ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            let mut reconciled = 0;
            for run_id in &run_ids {
                reconciled += store.recover_interrupted_attempts_tx(run_id)?;
            }
            // Safe pending work is ready to dispatch; uncertain side effects stay blocked.
            let auto_resumed: usize = store.conn.query_row(
                "SELECT COUNT(*) FROM runs WHERE auto_resume=1 AND state IN ('queued','recovering','paused') AND \
                 NOT EXISTS (SELECT 1 FROM tasks t WHERE t.run_id=runs.run_id AND t.state='uncertain')",
                [],
                |row| row.get(0),
            )?;
            if auto_resumed > 0 {
                store.conn.execute(
                    "UPDATE runs SET state='queued',reason_code=NULL,version=version+1,updated_ms=?1 \
                     WHERE auto_resume=1 AND state IN ('queued','recovering','paused') AND \
                     NOT EXISTS (SELECT 1 FROM tasks t WHERE t.run_id=runs.run_id AND t.state='uncertain')",
                    params![now_ms()],
                )?;
            }
            Ok(ReconciliationSummary {
                host_epoch: epoch,
                runs_scanned: run_ids.len(),
                changed_attempts: reconciled,
                auto_resumed,
            })
        })
    }

    pub fn quick_check(&self) -> StoreResult<()> {
        let ok: String = self
            .conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if ok != "ok" {
            return Err(ExecutionStoreError::Invariant(format!(
                "SQLite quick_check failed: {ok}"
            )));
        }
        Ok(())
    }

    pub fn create_run(
        &mut self,
        session_id: &str,
        branch_id: &str,
        input: serde_json::Value,
        env: serde_json::Value,
    ) -> StoreResult<RunSnapshot> {
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        let now = now_ms();
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        let result = self.create_run_tx(&run_id, session_id, branch_id, input, env, now);
        match result {
            Ok(()) => {
                self.conn.execute("COMMIT", [])?;
                self.snapshot(&run_id)
            }
            Err(e) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    fn create_run_tx(
        &self,
        run_id: &str,
        session_id: &str,
        branch_id: &str,
        input: serde_json::Value,
        env: serde_json::Value,
        now: i64,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO runs(run_id,session_id,branch_id,state,version,generation,next_event_seq,recovery_plan_id,desired_state,auto_resume,input_json,env_json,created_ms,updated_ms) \
             VALUES(?1,?2,?3,'queued',0,0,0,NULL,'running',0,?4,?5,?6,?6)",
            params![run_id, session_id, branch_id, input.to_string(), env.to_string(), now],
        )?;
        self.append_event_tx(
            run_id,
            0,
            "run.created",
            &serde_json::json!({ "runId": run_id }),
        )?;
        self.insert_outbox_tx(run_id, "schedule", &serde_json::json!({ "runId": run_id }))?;
        Ok(())
    }

    pub fn snapshot(&self, run_id: &str) -> StoreResult<RunSnapshot> {
        let run = self.load_run(run_id)?;
        let mut stmt = self
            .conn
            .prepare("SELECT task_id,logical_key,kind,state,version FROM tasks WHERE run_id=?1")?;
        let task_rows = stmt
            .query_map(params![run_id], |row| {
                Ok(TaskSummary {
                    task_id: row.get(0)?,
                    logical_key: row.get(1)?,
                    kind: row.get(2)?,
                    state: row.get(3)?,
                    version: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let cursor = ExecutionCursor {
            store_id: self.store_id.clone(),
            run_id: run_id.to_string(),
            seq: run.next_event_seq.saturating_sub(1),
        };
        let has_uncertain = task_rows.iter().any(|t| t.state == "uncertain");
        let resume_allowed = matches!(run.state.as_str(), "paused" | "queued" | "recovering")
            && !has_uncertain
            && run.auto_resume == 1;
        let reason = if has_uncertain {
            Some("uncertain_tool_effect".to_string())
        } else if !matches!(run.state.as_str(), "paused" | "queued" | "recovering") {
            Some(format!("run state is {}", run.state))
        } else if run.auto_resume == 0 {
            Some("auto_resume is disabled for this run".to_string())
        } else {
            None
        };
        Ok(RunSnapshot {
            run_id: run.run_id,
            session_id: run.session_id,
            branch_id: run.branch_id,
            state: run.state,
            reason_code: run.reason_code,
            version: run.version,
            generation: run.generation,
            recovery_plan_id: run.recovery_plan_id,
            cursor: ExecutionCursor {
                seq: cursor.seq.max(0),
                ..cursor
            },
            tasks: task_rows,
            actions: ResumeActions {
                resume_allowed,
                reason,
            },
        })
    }

    pub fn list_runs(&self, session_id: Option<&str>) -> StoreResult<Vec<RunListEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id,session_id,state,version,updated_ms FROM runs \
             WHERE (?1 IS NULL OR session_id=?1) ORDER BY updated_ms DESC LIMIT 200",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                Ok(RunListEntry {
                    run_id: row.get(0)?,
                    session_id: row.get(1)?,
                    state: row.get(2)?,
                    version: row.get(3)?,
                    updated_ms: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn resumable_runs(&self) -> StoreResult<Vec<RunSnapshot>> {
        let entries = self.list_runs(None)?;
        let mut snapshots = Vec::new();
        for entry in entries {
            if matches!(entry.state.as_str(), "queued" | "recovering") {
                snapshots.push(self.snapshot(&entry.run_id)?);
            }
        }
        Ok(snapshots)
    }

    pub fn set_run_state(
        &mut self,
        run_id: &str,
        state: &str,
        reason_code: Option<&str>,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            if !matches!(
                state,
                "queued"
                    | "running"
                    | "recovering"
                    | "waiting_children"
                    | "waiting_approval"
                    | "paused"
                    | "blocked"
                    | "succeeded"
                    | "failed"
                    | "cancelled"
            ) {
                return Err(ExecutionStoreError::Invariant(format!(
                    "invalid run state {state}"
                )));
            }
            let run = store.load_run(run_id)?;
            if TERMINAL_RUN_STATES.contains(&run.state.as_str()) {
                return Err(ExecutionStoreError::AlreadyTerminal {
                    run_id: run_id.to_string(),
                    state: run.state,
                });
            }
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state=?2,reason_code=?3,version=?4,updated_ms=?5 WHERE run_id=?1",
                params![run_id, state, reason_code, version, now_ms()],
            )?;
            store.append_event_tx(
                run_id,
                version,
                "run.state.changed",
                &serde_json::json!({
                    "state": state,
                    "reasonCode": reason_code,
                }),
            )?;
            Ok(())
        })
    }

    /// Pause a non-terminal run with a human-readable reason (SIGKILL simulation,
    /// user request, host shutdown, etc). Terminal runs cannot be paused.
    pub fn pause_run(&mut self, run_id: &str, reason: &str) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            if TERMINAL_RUN_STATES.contains(&run.state.as_str()) {
                return Err(ExecutionStoreError::AlreadyTerminal {
                    run_id: run_id.to_string(),
                    state: run.state,
                });
            }
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state='paused',reason_code=?2,version=?3,updated_ms=?4,auto_resume=1 WHERE run_id=?1",
                params![run_id, reason, version, now_ms()],
            )?;
            store.append_event_tx(
                run_id,
                version,
                "run.paused",
                &serde_json::json!({ "reason": reason }),
            )?;
            Ok(())
        })
    }

    /// Resume a paused run: clear reason and move to `recovering` so the next
    /// scheduler scan or explicit `accept_resume` can dispatch it.
    pub fn resume_run(&mut self, run_id: &str) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            if run.state != "paused" {
                return Err(ExecutionStoreError::Invariant(format!(
                    "run {run_id} is not paused (state={}), cannot resume",
                    run.state
                )));
            }
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state='recovering',reason_code=NULL,version=?2,updated_ms=?3 WHERE run_id=?1",
                params![run_id, version, now_ms()],
            )?;
            store.append_event_tx(
                run_id,
                version,
                "run.resumed",
                &serde_json::json!({ "state": "recovering" }),
            )?;
            Ok(())
        })
    }

    /// Toggle auto_resume for a run so the scheduler knows whether to pick it up.
    pub fn set_run_auto_resume(&mut self, run_id: &str, enabled: bool) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            if TERMINAL_RUN_STATES.contains(&run.state.as_str()) {
                return Err(ExecutionStoreError::AlreadyTerminal {
                    run_id: run_id.to_string(),
                    state: run.state,
                });
            }
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET auto_resume=?2,version=?3,updated_ms=?4 WHERE run_id=?1",
                params![run_id, enabled as i64, version, now_ms()],
            )?;
            store.append_event_tx(
                run_id,
                version,
                "run.auto_resume.changed",
                &serde_json::json!({ "enabled": enabled }),
            )?;
            Ok(())
        })
    }

    fn load_run(&self, run_id: &str) -> StoreResult<RunRow> {
        self.conn
            .query_row(
                "SELECT run_id,session_id,branch_id,state,reason_code,version,generation,next_event_seq,recovery_plan_id,auto_resume FROM runs WHERE run_id=?1",
                params![run_id],
                RunRow::from_row,
            )
            .optional()?
            .ok_or_else(|| ExecutionStoreError::NotFound(run_id.to_string()))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn accept_resume(
        &mut self,
        principal: &str,
        run_id: &str,
        request_id: &str,
        expected_version: i64,
        recovery_plan_id: Option<&str>,
        request_payload: &serde_json::Value,
    ) -> StoreResult<ResumeReceipt> {
        let request_hash = hash_value(request_payload);
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        match self.accept_resume_tx(
            principal,
            run_id,
            request_id,
            expected_version,
            recovery_plan_id,
            request_hash,
        ) {
            Ok(op) => {
                self.conn.execute("COMMIT", [])?;
                Ok(op)
            }
            Err(e) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_resume_tx(
        &self,
        principal: &str,
        run_id: &str,
        request_id: &str,
        expected_version: i64,
        recovery_plan_id: Option<&str>,
        request_hash: String,
    ) -> StoreResult<ResumeReceipt> {
        let run = self.load_run(run_id)?;
        if let Some((existing_hash, operation_id, accepted_version, state)) = self
            .conn
            .query_row(
                "SELECT request_hash,operation_id,accepted_version,state FROM resume_ops WHERE principal_id=?1 AND run_id=?2 AND request_id=?3",
                params![principal, run_id, request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?)),
            )
            .optional()?
        {
            if existing_hash != request_hash {
                return Err(ExecutionStoreError::IdempotencyConflict {
                    request_id: request_id.to_string(),
                });
            }
            return Ok(ResumeReceipt {
                status: state,
                operation_id,
                accepted_version,
            });
        }
        if TERMINAL_RUN_STATES.contains(&run.state.as_str()) {
            return Err(ExecutionStoreError::AlreadyTerminal {
                run_id: run_id.to_string(),
                state: run.state,
            });
        }
        if run.version != expected_version {
            return Err(ExecutionStoreError::VersionConflict {
                expected: expected_version,
                current: run.version,
            });
        }
        if let Some((operation_id, _)) = self
            .conn
            .query_row(
                "SELECT operation_id,state FROM resume_ops WHERE run_id=?1 AND state IN ('accepted','reconciling','dispatching') LIMIT 1",
                params![run_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            return Err(ExecutionStoreError::AlreadyInProgress {
                run_id: run_id.to_string(),
                operation_id,
            });
        }
        if let Some(expected_plan) = recovery_plan_id
            && expected_plan != run.recovery_plan_id.as_deref().unwrap_or("")
        {
            return Err(ExecutionStoreError::VersionConflict {
                expected: -1,
                current: run.version,
            });
        }
        let operation_id = format!("resume-{}", uuid::Uuid::new_v4());
        let accepted_json = serde_json::json!({
            "principalId": principal,
            "runId": run_id,
            "requestId": request_id,
            "recoveryPlanId": recovery_plan_id,
        })
        .to_string();
        self.conn.execute(
            "INSERT INTO resume_ops(operation_id,principal_id,run_id,request_id,request_hash,state,accepted_json,accepted_version) VALUES(?1,?2,?3,?4,?5,'accepted',?6,?7)",
            params![operation_id, principal, run_id, request_id, request_hash, accepted_json, expected_version + 1],
        )?;
        self.conn.execute(
            "UPDATE runs SET state=CASE WHEN state='paused' THEN 'recovering' ELSE state END,version=version+1,next_event_seq=next_event_seq+1,recovery_plan_id=?2,updated_ms=?3 WHERE run_id=?1",
            params![run_id, recovery_plan_id, now_ms()],
        )?;
        self.append_event_tx(
            run_id,
            expected_version + 1,
            "run.resume.accepted",
            &serde_json::json!({ "operationId": operation_id }),
        )?;
        self.insert_outbox_tx(
            run_id,
            "resume",
            &serde_json::json!({ "operationId": operation_id }),
        )?;
        Ok(ResumeReceipt {
            status: "accepted".into(),
            operation_id,
            accepted_version: expected_version + 1,
        })
    }

    pub fn events_after(
        &self,
        run_id: &str,
        after_seq: i64,
        limit: usize,
    ) -> StoreResult<EventPage> {
        let run = self.load_run(run_id)?;
        if after_seq < 0 || after_seq >= run.next_event_seq {
            // A new run has no event yet: cursor 0 is valid and returns an empty page.
            if after_seq == 0 && run.next_event_seq == 0 {
                return Ok(EventPage {
                    cursor: ExecutionCursor {
                        store_id: self.store_id.clone(),
                        run_id: run_id.to_string(),
                        seq: 0,
                    },
                    events: Vec::new(),
                });
            }
            return Err(ExecutionStoreError::InvalidCursor {
                store_id: self.store_id.clone(),
                seq: after_seq,
            });
        }
        let mut stmt = self.conn.prepare(
            "SELECT seq,run_version,kind,payload_json FROM run_events WHERE run_id=?1 AND seq>=?2 ORDER BY seq LIMIT ?3",
        )?;
        let events = stmt
            .query_map(params![run_id, after_seq, limit as i64], |row| {
                let seq = row.get::<_, i64>(0)?;
                let kind = row.get::<_, String>(2)?;
                let payload = row.get::<_, String>(3)?;
                let mut value = serde_json::from_str::<serde_json::Value>(&payload)
                    .unwrap_or(serde_json::Value::Null);
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("kind".into(), serde_json::Value::String(kind));
                    obj.insert("seq".into(), serde_json::Value::from(seq));
                }
                Ok(value)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EventPage {
            cursor: ExecutionCursor {
                store_id: self.store_id.clone(),
                run_id: run_id.to_string(),
                seq: events
                    .last()
                    .map(|_| after_seq + events.len() as i64)
                    .unwrap_or(after_seq),
            },
            events,
        })
    }

    pub fn checkpoint_tool_batch(
        &mut self,
        run_id: &str,
        response: &serde_json::Value,
        tools: &[ToolCheckpoint],
    ) -> StoreResult<Vec<String>> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            if TERMINAL_RUN_STATES.contains(&run.state.as_str()) {
                return Err(ExecutionStoreError::AlreadyTerminal {
                    run_id: run_id.to_string(),
                    state: run.state,
                });
            }
            let version = run.version + 1;
            let checkpoint = serde_json::json!({
                "response": response,
                "toolCount": tools.len(),
            });
            let mut task_ids = Vec::with_capacity(tools.len());
            for tool in tools {
                store.conn.execute(
                    "INSERT INTO tasks(task_id,run_id,parent_task_id,logical_key,kind,state,version,checkpoint_version,checkpoint_json,result_json,policy_json) \
                     VALUES(?1,?2,NULL,?3,'tool','pending',0,0,?4,NULL,?5)",
                    params![
                        tool.task_id,
                        run_id,
                        tool.logical_key,
                        checkpoint.to_string(),
                        serde_json::to_string(&tool.policy).unwrap_or_else(|_| "\"unsafe\"".into()),
                    ],
                )?;
                task_ids.push(tool.task_id.clone());
            }
            store.conn.execute(
                "UPDATE runs SET state='running',version=?2,updated_ms=?3 WHERE run_id=?1",
                params![run_id, version, now_ms()],
            )?;
            store.append_event_tx(run_id, version, "tool.batch.checkpointed", &serde_json::json!({
                "response": response,
                "toolCount": tools.len(),
                "taskIds": task_ids,
            }))?;
            store.insert_outbox_tx(run_id, "dispatch_tools", &serde_json::json!({ "taskIds": task_ids }))?;
            Ok(task_ids)
        })
    }

    pub fn checkpoint_child_run(
        &mut self,
        parent_run_id: &str,
        parent_task_id: &str,
        plan: &ChildRunPlan,
    ) -> StoreResult<String> {
        self.transaction(|store| {
            let parent = store.load_run(parent_run_id)?;
            let existing: Option<String> = store
                .conn
                .query_row(
                    "SELECT child_id FROM task_edges WHERE parent_id=?1 AND edge_key=?2",
                    params![parent_task_id, plan.spawn_key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(child_id) = existing {
                return Ok(child_id);
            }
            let child_id = format!("task-{}", uuid::Uuid::new_v4());
            store.conn.execute(
                "INSERT INTO tasks(task_id,run_id,parent_task_id,logical_key,kind,state,version,checkpoint_version,checkpoint_json,result_json,policy_json) \
                 VALUES(?1,?2,?3,?4,'child_run','waiting',0,0,?5,NULL,'\"unsafe\"')",
                params![
                    child_id,
                    parent_run_id,
                    parent_task_id,
                    format!("spawn:{}", plan.spawn_key),
                    serde_json::json!({ "input": plan.input, "sessionId": plan.session_id }).to_string(),
                ],
            )?;
            store.conn.execute(
                "INSERT INTO task_edges(parent_id,child_id,edge_key,ownership,wait_policy) VALUES(?1,?2,?3,'owned','all_settled')",
                params![parent_task_id, child_id, plan.spawn_key],
            )?;
            store.conn.execute(
                "INSERT INTO queued_inputs(input_id,run_id,payload_json,consumed_by_task) VALUES(?1,?2,?3,?4)",
                params![format!("input-{}", uuid::Uuid::new_v4()), parent_run_id, plan.input.to_string(), child_id],
            )?;
            let version = parent.version + 1;
            store.conn.execute(
                "UPDATE runs SET state='waiting_children',version=?2,updated_ms=?3 WHERE run_id=?1",
                params![parent_run_id, version, now_ms()],
            )?;
            store.append_event_tx(parent_run_id, version, "child.checkpointed", &serde_json::json!({
                "parentTaskId": parent_task_id,
                "childTaskId": child_id,
                "spawnKey": plan.spawn_key,
            }))?;
            store.insert_outbox_tx(parent_run_id, "spawn_child", &serde_json::json!({ "childTaskId": child_id }))?;
            Ok(child_id)
        })
    }

    pub fn consume_child_input(
        &mut self,
        run_id: &str,
        child_task_id: &str,
        output: &serde_json::Value,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            let input_id: Option<String> = store.conn.query_row(
                "SELECT input_id FROM queued_inputs WHERE run_id=?1 AND consumed_by_task=?2",
                params![run_id, child_task_id],
                |row| row.get(0),
            )
            .optional()?;
            let Some(input_id) = input_id else {
                return Err(ExecutionStoreError::Invariant(
                    "child input already consumed or missing".into(),
                ));
            };
            store.conn.execute(
                "UPDATE tasks SET state='succeeded',version=version+1,result_json=?2 WHERE task_id=?1",
                params![child_task_id, output.to_string()],
            )?;
            store.conn.execute(
                "DELETE FROM queued_inputs WHERE input_id=?1",
                params![input_id],
            )?;
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state='running',reason_code=NULL,version=?2,updated_ms=?3 WHERE run_id=?1",
                params![run_id, version, now_ms()],
            )?;
            store.append_event_tx(run_id, version, "child.completed", &serde_json::json!({
                "childTaskId": child_task_id,
                "inputId": input_id,
            }))?;
            Ok(())
        })
    }

    pub fn checkpoint_approval(
        &mut self,
        run_id: &str,
        approval: &ApprovalCheckpoint,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            if approval.generation != run.generation {
                return Err(ExecutionStoreError::Invariant(format!(
                    "approval generation {} != run generation {}",
                    approval.generation, run.generation
                )));
            }
            store.conn.execute(
                "UPDATE approvals SET status='stale',version=version+1 WHERE run_id=?1 AND task_id=?2 AND status='pending' AND (generation<>?3 OR policy_version<>?4 OR action_digest<>?5)",
                params![run_id, approval.task_id, approval.generation, approval.policy_version, approval.action_digest],
            )?;
            store.conn.execute(
                "INSERT INTO approvals(approval_id,run_id,task_id,action_digest,generation,policy_version,status,expires_ms,decision_json,version) \
                 VALUES(?1,?2,?3,?4,?5,?6,'pending',NULL,NULL,0) \
                 ON CONFLICT(approval_id) DO UPDATE SET action_digest=excluded.action_digest,generation=excluded.generation,policy_version=excluded.policy_version,status='pending',version=approvals.version+1",
                params![approval.approval_id, run_id, approval.task_id, approval.action_digest, approval.generation, approval.policy_version],
            )?;
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state='waiting_approval',version=?2,updated_ms=?3 WHERE run_id=?1",
                params![run_id, version, now_ms()],
            )?;
            store.append_event_tx(run_id, version, "approval.pending", &serde_json::json!({
                "approvalId": approval.approval_id,
                "taskId": approval.task_id,
            }))?;
            Ok(())
        })
    }

    pub fn consume_approval(
        &mut self,
        run_id: &str,
        approval_id: &str,
        action_digest: &str,
        generation: i64,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            let changed = store.conn.execute(
                "UPDATE approvals SET status='consumed',version=version+1 WHERE approval_id=?1 AND run_id=?2 AND action_digest=?3 AND generation=?4 AND status='approved'",
                params![approval_id, run_id, action_digest, generation],
            )?;
            if changed != 1 {
                return Err(ExecutionStoreError::Invariant("stale_approval".into()));
            }
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state='running',version=?2,updated_ms=?3 WHERE run_id=?1",
                params![run_id, version, now_ms()],
            )?;
            store.append_event_tx(run_id, version, "approval.consumed", &serde_json::json!({
                "approvalId": approval_id,
            }))?;
            Ok(())
        })
    }

    pub fn decide_approval(
        &mut self,
        run_id: &str,
        approval_id: &str,
        approve: bool,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            let changed = store.conn.execute(
                "UPDATE approvals SET status=?3,decision_json=?4,version=version+1 \
                 WHERE approval_id=?1 AND run_id=?2 AND status='pending'",
                params![
                    approval_id,
                    run_id,
                    if approve { "approved" } else { "rejected" },
                    serde_json::json!({ "approve": approve }).to_string()
                ],
            )?;
            if changed != 1 {
                return Err(ExecutionStoreError::Invariant("stale_approval".into()));
            }
            let version = run.version + 1;
            store.conn.execute(
                "UPDATE runs SET state=?2,version=?3,updated_ms=?4 WHERE run_id=?1",
                params![
                    run_id,
                    if approve { "running" } else { "failed" },
                    version,
                    now_ms()
                ],
            )?;
            store.append_event_tx(
                run_id,
                version,
                "approval.decided",
                &serde_json::json!({
                    "approvalId": approval_id,
                    "approve": approve,
                }),
            )?;
            Ok(())
        })
    }

    pub fn start_tool_attempt(
        &mut self,
        run_id: &str,
        task_id: &str,
        worker_id: Option<&str>,
    ) -> StoreResult<AttemptLease> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            let (_, state): (String, String) = store.conn.query_row(
                "SELECT policy_json,state FROM tasks WHERE task_id=?1 AND run_id=?2",
                params![task_id, run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if state != "pending" {
                return Err(ExecutionStoreError::Invariant(format!(
                    "task {task_id} is {state}, expected pending"
                )));
            }
            let attempt_no: i64 = store.conn.query_row(
                "SELECT COALESCE(MAX(attempt_no)+1,0) FROM attempts WHERE task_id=?1",
                params![task_id],
                |row| row.get(0),
            )?;
            let attempt_id = format!("attempt-{}", uuid::Uuid::new_v4());
            let lease = AttemptLease {
                task_id: task_id.to_string(),
                attempt_id: attempt_id.clone(),
                attempt_no,
                host_epoch: 0,
                generation: run.generation,
                fence: attempt_no,
            };
            store.conn.execute(
                "INSERT INTO attempts(attempt_id,task_id,attempt_no,host_epoch,generation,fence,worker_id,phase,started_ms,lease_deadline_ms,outcome_json) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'executing',?8,NULL,NULL)",
                params![attempt_id, task_id, attempt_no, lease.host_epoch, lease.generation, lease.fence, worker_id, now_ms()],
            )?;
            store.conn.execute(
                "UPDATE tasks SET state='running',version=version+1 WHERE task_id=?1",
                params![task_id],
            )?;
            store.conn.execute(
                "UPDATE runs SET version=version+1,updated_ms=?2 WHERE run_id=?1",
                params![run_id, now_ms()],
            )?;
            store.append_event_tx(run_id, run.version + 1, "tool.started", &serde_json::json!({
                "taskId": task_id,
                "attemptId": attempt_id,
            }))?;
            Ok(lease)
        })
    }

    pub fn complete_tool_attempt(
        &mut self,
        run_id: &str,
        lease: &AttemptLease,
        outcome: &ToolOutcome,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            let run = store.load_run(run_id)?;
            let changed = store.conn.execute(
                "UPDATE attempts SET phase=?4,outcome_json=?5 WHERE attempt_id=?1 AND task_id=?2 AND attempt_no=?3 AND host_epoch=?6 AND generation=?7 AND fence=?8 AND phase='executing'",
                params![lease.attempt_id, lease.task_id, lease.attempt_no, if outcome.success {"succeeded"} else {"failed"}, serde_json::to_string(outcome).unwrap_or_default(), lease.host_epoch, lease.generation, lease.fence],
            )?;
            if changed != 1 {
                return Err(ExecutionStoreError::Invariant(format!(
                    "attempt {} is no longer the active fenced attempt",
                    lease.attempt_id
                )));
            }
            let result = serde_json::json!({ "output": outcome.output, "success": outcome.success });
            store.conn.execute(
                "UPDATE tasks SET state=?2,version=version+1,result_json=?3 WHERE task_id=?1",
                params![lease.task_id, if outcome.success {"succeeded"} else {"failed"}, result.to_string()],
            )?;
            store.conn.execute(
                "INSERT INTO transcript_entries(entry_id,run_id,ordinal,payload_json) VALUES(?1,?2,?3,?4)",
                params![format!("entry-{}", uuid::Uuid::new_v4()), run_id, next_transcript_ordinal(&store.conn, run_id)?, result.to_string()],
            )?;
            store.conn.execute(
                "UPDATE runs SET version=version+1,updated_ms=?2 WHERE run_id=?1",
                params![run_id, now_ms()],
            )?;
            store.append_event_tx(run_id, run.version + 2, "tool.completed", &serde_json::json!({
                "taskId": lease.task_id,
                "attemptId": lease.attempt_id,
                "success": outcome.success,
            }))?;
            Ok(())
        })
    }

    pub fn recover_interrupted_attempts(&mut self, run_id: &str) -> StoreResult<usize> {
        self.transaction(|store| store.recover_interrupted_attempts_tx(run_id))
    }

    pub fn auto_resume_pending(&mut self) -> StoreResult<Vec<String>> {
        self.transaction(|store| {
            let mut stmt = store.conn.prepare(
                "SELECT run_id FROM runs WHERE auto_resume=1 AND state IN ('queued','recovering') AND \
                 NOT EXISTS (SELECT 1 FROM tasks t WHERE t.run_id=runs.run_id AND t.state='uncertain') \
                 ORDER BY updated_ms ASC LIMIT 100",
            )?;
            let run_ids = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);
            for run_id in &run_ids {
                let run = store.load_run(run_id)?;
                let version = run.version + 1;
                store.conn.execute(
                    "UPDATE runs SET state='queued',reason_code=NULL,version=?2,updated_ms=?3 WHERE run_id=?1",
                    params![run_id, version, now_ms()],
                )?;
                store.append_event_tx(
                    run_id,
                    version,
                    "run.auto_resume",
                    &serde_json::json!({ "runId": run_id }),
                )?;
                store.insert_outbox_tx(
                    run_id,
                    "schedule",
                    &serde_json::json!({ "runId": run_id }),
                )?;
            }
            Ok(run_ids)
        })
    }

    fn recover_interrupted_attempts_tx(&self, run_id: &str) -> StoreResult<usize> {
        {
            let mut stmt = self.conn.prepare(
                "SELECT a.task_id,a.attempt_id,t.policy_json FROM attempts a \
                 JOIN tasks t ON t.task_id=a.task_id \
                 WHERE t.run_id=?1 AND a.phase='executing' AND t.result_json IS NULL",
            )?;
            let rows = stmt
                .query_map(params![run_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut changed = 0;
            let mut blocked = false;
            for (task_id, attempt_id, policy_json) in rows {
                let replay_safe = policy_json.contains("replay_safe");
                let phase = if replay_safe { "failed" } else { "uncertain" };
                let task_state = if replay_safe { "pending" } else { "uncertain" };
                let outcome = serde_json::json!({
                    "interrupted": true,
                    "replaySafe": replay_safe,
                })
                .to_string();
                self.conn.execute(
                    "UPDATE attempts SET phase=?2,outcome_json=?3 WHERE attempt_id=?1 AND phase='executing'",
                    params![attempt_id, phase, outcome],
                )?;
                self.conn.execute(
                    "UPDATE tasks SET state=?2,version=version+1 WHERE task_id=?1",
                    params![task_id, task_state],
                )?;
                changed += 1;
                if !replay_safe {
                    blocked = true;
                }
            }
            if changed > 0 {
                let run = self.load_run(run_id)?;
                let version = run.version + 1;
                let state = if blocked {
                    "blocked".to_string()
                } else if self.conn.query_row(
                    "SELECT auto_resume FROM runs WHERE run_id=?1",
                    params![run_id],
                    |row| row.get::<_, i64>(0),
                )? == 1
                {
                    "queued".to_string()
                } else {
                    run.state
                };
                let reason = if blocked {
                    Some("uncertain_tool_effect".to_string())
                } else {
                    None
                };
                self.conn.execute(
                    "UPDATE runs SET state=?2,reason_code=?3,version=?4,updated_ms=?5 WHERE run_id=?1",
                    params![run_id, state, reason, version, now_ms()],
                )?;
                self.append_event_tx(
                    run_id,
                    version,
                    "run.recovery.reconciled",
                    &serde_json::json!({
                        "changedAttempts": changed,
                        "blocked": blocked,
                    }),
                )?;
            }
            Ok(changed)
        }
    }

    fn transaction<T>(&mut self, f: impl FnOnce(&Self) -> StoreResult<T>) -> StoreResult<T> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        match f(self) {
            Ok(value) => {
                self.conn.execute("COMMIT", [])?;
                Ok(value)
            }
            Err(e) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    fn transaction_mut<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> StoreResult<T>,
    ) -> StoreResult<T> {
        self.conn.execute("BEGIN IMMEDIATE", [])?;
        match f(self) {
            Ok(value) => {
                self.conn.execute("COMMIT", [])?;
                Ok(value)
            }
            Err(e) => {
                let _ = self.conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    fn append_event_tx(
        &self,
        run_id: &str,
        run_version: i64,
        kind: &str,
        payload: &serde_json::Value,
    ) -> StoreResult<()> {
        let seq: i64 = self.conn.query_row(
            "SELECT next_event_seq FROM runs WHERE run_id=?1",
            params![run_id],
            |row| row.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO run_events(run_id,seq,run_version,kind,payload_json) VALUES(?1,?2,?3,?4,?5)",
            params![run_id, seq, run_version, kind, payload.to_string()],
        )?;
        self.conn.execute(
            "UPDATE runs SET next_event_seq=?2 WHERE run_id=?1",
            params![run_id, seq + 1],
        )?;
        Ok(())
    }

    fn insert_outbox_tx(
        &self,
        run_id: &str,
        kind: &str,
        payload: &serde_json::Value,
    ) -> StoreResult<()> {
        self.conn.execute(
            "INSERT INTO outbox(outbox_id,run_id,effect_key,kind,payload_json) VALUES(?1,?2,?3,?4,?5)",
            params![
                format!("outbox-{}", uuid::Uuid::new_v4()),
                run_id,
                format!("{kind}:{}", uuid::Uuid::new_v4()),
                kind,
                payload.to_string()
            ],
        )?;
        Ok(())
    }

    pub fn assert_reopen_preserves_store_id(&self) -> StoreResult<String> {
        self.conn
            .query_row(
                "SELECT store_id FROM store_meta WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .map_err(Into::into)
    }

    /// Pull unprojected transcript entries. Delivery happens outside the ledger;
    /// mark_transcript_projected must only run after the JSONL append succeeds.
    pub fn pending_transcripts(&self) -> StoreResult<Vec<TranscriptProjection>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.entry_id,t.run_id,r.session_id,t.payload_json FROM transcript_entries t \
             JOIN runs r ON r.run_id=t.run_id \
             LEFT JOIN outbox o ON o.run_id=t.run_id AND o.kind='project_transcript' \
               AND json_extract(o.payload_json,'$.entryId')=t.entry_id \
             WHERE o.outbox_id IS NULL ORDER BY t.run_id,t.ordinal",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let payload = row.get::<_, String>(3)?;
                Ok(TranscriptProjection {
                    entry_id: row.get(0)?,
                    run_id: row.get(1)?,
                    session_id: row.get(2)?,
                    payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn mark_transcript_projected(
        &mut self,
        entry_id: &str,
        projected: bool,
    ) -> StoreResult<()> {
        self.transaction(|store| {
            let transcript: (String, String) = store.conn.query_row(
                "SELECT entry_id,run_id FROM transcript_entries WHERE entry_id=?1",
                params![entry_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if projected {
                store.conn.execute(
                    "INSERT INTO outbox(outbox_id,run_id,effect_key,kind,payload_json,delivered_ms) \
                     VALUES(?1,?2,?3,'project_transcript',?4,?5) \
                     ON CONFLICT(effect_key) DO UPDATE SET delivered_ms=excluded.delivered_ms",
                    params![
                        format!("outbox-{}", uuid::Uuid::new_v4()),
                        transcript.1,
                        format!("project_transcript:{entry_id}"),
                        serde_json::json!({ "entryId": entry_id }).to_string(),
                        now_ms()
                    ],
                )?;
            } else {
                store.conn.execute(
                    "DELETE FROM outbox WHERE kind='project_transcript' AND json_extract(payload_json,'$.entryId')=?1",
                    params![entry_id],
                )?;
            }
            Ok(())
        })
    }
}

struct RunRow {
    run_id: String,
    session_id: String,
    branch_id: String,
    state: String,
    reason_code: Option<String>,
    version: i64,
    generation: i64,
    next_event_seq: i64,
    recovery_plan_id: Option<String>,
    auto_resume: i64,
}

impl RunRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            run_id: row.get(0)?,
            session_id: row.get(1)?,
            branch_id: row.get(2)?,
            state: row.get(3)?,
            reason_code: row.get(4)?,
            version: row.get(5)?,
            generation: row.get(6)?,
            next_event_seq: row.get(7)?,
            recovery_plan_id: row.get(8)?,
            auto_resume: row.get(9)?,
        })
    }
}

pub struct ExecutionWriter {
    tx: Sender<WriterRequest>,
}

struct WriterRequest {
    callback: Box<dyn FnOnce(&mut ExecutionStore) + Send + 'static>,
}

impl ExecutionWriter {
    pub fn spawn(store: ExecutionStore) -> Self {
        let (tx, rx) = channel::<WriterRequest>();
        std::thread::Builder::new()
            .name("ion-execution-writer".into())
            .spawn(move || {
                let mut store = store;
                for req in rx {
                    (req.callback)(&mut store);
                }
            })
            .expect("spawn execution writer");
        Self { tx }
    }

    pub fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut ExecutionStore) -> StoreResult<T> + Send + 'static,
    ) -> StoreResult<T> {
        let (reply_tx, reply_rx) = channel::<StoreResult<T>>();
        if self
            .tx
            .send(WriterRequest {
                callback: Box::new(move |store| {
                    let _ = reply_tx.send(f(store));
                }),
            })
            .is_err()
        {
            return Err(ExecutionStoreError::Storage(
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    "writer stopped",
                ))),
            ));
        }
        reply_rx.recv().map_err(|e| {
            ExecutionStoreError::Storage(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?
    }

    pub async fn call_async<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&mut ExecutionStore) -> StoreResult<T> + Send + 'static,
    ) -> StoreResult<T> {
        let writer = Arc::clone(self);
        tokio::task::spawn_blocking(move || writer.call(f))
            .await
            .map_err(|e| {
                ExecutionStoreError::Storage(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            })?
    }
}

pub fn shared_writer(path: impl AsRef<Path>) -> StoreResult<Arc<ExecutionWriter>> {
    static WRITER: OnceLock<Arc<ExecutionWriter>> = OnceLock::new();
    if let Some(existing) = WRITER.get() {
        return Ok(Arc::clone(existing));
    }
    let writer = Arc::new(ExecutionWriter::spawn(ExecutionStore::open(path)?));
    Ok(Arc::clone(WRITER.get_or_init(|| writer)))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn next_transcript_ordinal(conn: &Connection, run_id: &str) -> StoreResult<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(ordinal)+1,0) FROM transcript_entries WHERE run_id=?1",
        params![run_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn hash_value(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let raw = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(raw);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (ExecutionStore, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("execution.sqlite3");
        (ExecutionStore::open(&path).unwrap(), dir)
    }

    #[test]
    fn pause_run_records_reason_and_rejects_when_terminal() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run(
                "s-pause",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();
        store.pause_run(&snap.run_id, "simulated SIGKILL").unwrap();
        let paused = store.snapshot(&snap.run_id).unwrap();
        assert_eq!(paused.state, "paused");
        assert_eq!(paused.reason_code.as_deref(), Some("simulated SIGKILL"));
        assert!(paused.actions.resume_allowed);

        store
            .set_run_state(&snap.run_id, "failed", Some("test"))
            .unwrap();
        let err = store.pause_run(&snap.run_id, "another pause").unwrap_err();
        assert!(matches!(err, ExecutionStoreError::AlreadyTerminal { .. }));
    }

    #[test]
    fn resume_after_pause_clears_reason_and_returns_to_recovering() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run(
                "s-resume",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();
        store.pause_run(&snap.run_id, "manual pause").unwrap();
        assert!(store.snapshot(&snap.run_id).unwrap().actions.resume_allowed);

        store.resume_run(&snap.run_id).unwrap();
        let recovered = store.snapshot(&snap.run_id).unwrap();
        assert_eq!(recovered.state, "recovering");
        assert!(recovered.reason_code.is_none());
    }

    #[test]
    fn snapshot_actions_reflect_uncertain_blocked_and_active() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run(
                "s-actions",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();

        // Fresh queued run with auto_resume=0 → no resume action.
        let queued = store.snapshot(&snap.run_id).unwrap();
        assert!(!queued.actions.resume_allowed);
        assert!(
            queued
                .actions
                .reason
                .as_deref()
                .unwrap()
                .contains("auto_resume")
        );

        // Enable auto_resume → resumable.
        store.set_run_auto_resume(&snap.run_id, true).unwrap();
        let enabled = store.snapshot(&snap.run_id).unwrap();
        assert!(enabled.actions.resume_allowed);

        // Set blocked with uncertain task → resume not allowed.
        store
            .set_run_state(&snap.run_id, "blocked", Some("uncertain_tool_effect"))
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO tasks(task_id,run_id,parent_task_id,logical_key,kind,state,version,checkpoint_version,checkpoint_json,result_json,policy_json) \
                 VALUES('task-uncertain',?1,NULL,'call:0','tool','uncertain',0,0,'{}',NULL,'\"unsafe\"')",
                params![snap.run_id],
            )
            .unwrap();
        let blocked = store.snapshot(&snap.run_id).unwrap();
        assert!(!blocked.actions.resume_allowed);
        assert_eq!(
            blocked.actions.reason.as_deref(),
            Some("uncertain_tool_effect")
        );

        // Running run → not resumable.
        store
            .conn
            .execute("DELETE FROM tasks WHERE run_id=?1", params![snap.run_id])
            .unwrap();
        store.set_run_state(&snap.run_id, "running", None).unwrap();
        let running = store.snapshot(&snap.run_id).unwrap();
        assert!(!running.actions.resume_allowed);
    }

    #[test]
    fn create_run_returns_transactional_snapshot_and_event() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run(
                "s1",
                "main",
                serde_json::json!({ "prompt": "x" }),
                serde_json::json!({}),
            )
            .unwrap();
        assert_eq!(snap.state, "queued");
        assert_eq!(snap.version, 0);
        assert_eq!(snap.cursor.seq, 0);
        let page = store.events_after(&snap.run_id, 0, 10).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0]["runId"], snap.run_id);
        assert_eq!(page.events[0]["kind"], "run.created");
        assert_eq!(page.events[0]["seq"], 0);
        assert_eq!(page.cursor.seq, 1);
    }

    #[test]
    fn list_runs_filters_by_session_and_orders_recent_first() {
        let (mut store, _dir) = temp_store();
        let a = store
            .create_run(
                "session-a",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();
        let b = store
            .create_run(
                "session-b",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();
        let all = store.list_runs(None).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|run| run.run_id == a.run_id));
        assert!(all.iter().any(|run| run.run_id == b.run_id));
        let only_a = store.list_runs(Some("session-a")).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].run_id, a.run_id);
    }

    #[test]
    fn transcript_projection_is_idempotent_after_ack() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let checkpoint = ToolCheckpoint {
            task_id: "safe-task".into(),
            logical_key: "call".into(),
            name: "read".into(),
            call_id: "call".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::ReplaySafe,
        };
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({}),
                std::slice::from_ref(&checkpoint),
            )
            .unwrap();
        let lease = store
            .start_tool_attempt(&snap.run_id, "safe-task", None)
            .unwrap();
        store
            .complete_tool_attempt(
                &snap.run_id,
                &lease,
                &ToolOutcome {
                    task_id: "safe-task".into(),
                    success: true,
                    output: "projected once".into(),
                },
            )
            .unwrap();

        let pending = store.pending_transcripts().unwrap();
        assert_eq!(pending.len(), 1);
        let id = pending[0].entry_id.clone();
        store.mark_transcript_projected(&id, true).unwrap();
        assert!(store.pending_transcripts().unwrap().is_empty());
        // Re-ack is idempotent and rollback removes delivery state for retry.
        store.mark_transcript_projected(&id, true).unwrap();
        store.mark_transcript_projected(&id, false).unwrap();
        assert_eq!(store.pending_transcripts().unwrap().len(), 1);
    }

    #[test]
    fn scheduler_auto_resumes_safe_pending_and_blocks_uncertain() {
        let (mut store, _dir) = temp_store();
        let safe = store
            .create_run("safe", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let unsafe_run = store
            .create_run(
                "unsafe",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET auto_resume=1,state='recovering' WHERE run_id=?1",
                params![safe.run_id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE runs SET auto_resume=1,state='recovering' WHERE run_id=?1",
                params![unsafe_run.run_id],
            )
            .unwrap();
        let safe_task = ToolCheckpoint {
            task_id: "safe-task".into(),
            logical_key: "safe".into(),
            name: "read".into(),
            call_id: "safe-call".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::ReplaySafe,
        };
        let unsafe_task = ToolCheckpoint {
            task_id: "unsafe-task".into(),
            logical_key: "unsafe".into(),
            name: "bash".into(),
            call_id: "unsafe-call".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::Unsafe,
        };
        store
            .checkpoint_tool_batch(&safe.run_id, &serde_json::json!({}), &[safe_task])
            .unwrap();
        store
            .checkpoint_tool_batch(&unsafe_run.run_id, &serde_json::json!({}), &[unsafe_task])
            .unwrap();
        store
            .start_tool_attempt(&safe.run_id, "safe-task", None)
            .unwrap();
        store
            .start_tool_attempt(&unsafe_run.run_id, "unsafe-task", None)
            .unwrap();
        store.recover_interrupted_attempts(&safe.run_id).unwrap();
        store
            .recover_interrupted_attempts(&unsafe_run.run_id)
            .unwrap();

        let resumed = store.auto_resume_pending().unwrap();
        assert_eq!(resumed, vec![safe.run_id.clone()]);
        assert_eq!(store.snapshot(&safe.run_id).unwrap().state, "queued");
        let blocked = store.snapshot(&unsafe_run.run_id).unwrap();
        assert_eq!(blocked.state, "blocked");
        assert_eq!(
            blocked.reason_code.as_deref(),
            Some("uncertain_tool_effect")
        );
    }

    #[test]
    fn resume_is_idempotent_and_version_fenced() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let payload = serde_json::json!({ "requestId": "r1" });
        let first = store
            .accept_resume("user", &snap.run_id, "r1", 0, None, &payload)
            .unwrap();
        assert_eq!(first.status, "accepted");
        let second = store
            .accept_resume(
                "user",
                &snap.run_id,
                "r1",
                first.accepted_version,
                None,
                &payload,
            )
            .unwrap();
        assert_eq!(first.operation_id, second.operation_id);
        let changed = store
            .accept_resume(
                "user",
                &snap.run_id,
                "r1",
                first.accepted_version,
                None,
                &serde_json::json!("different"),
            )
            .unwrap_err();
        assert!(matches!(
            changed,
            ExecutionStoreError::IdempotencyConflict { .. }
        ));
        let stale = store
            .accept_resume("user", &snap.run_id, "r2", 0, None, &payload)
            .unwrap_err();
        assert!(matches!(stale, ExecutionStoreError::VersionConflict { .. }));
    }

    #[test]
    fn reopen_preserves_store_identity_and_schema() {
        let (mut store, dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let original_id = store.assert_reopen_preserves_store_id().unwrap();
        drop(store);
        let reopened = ExecutionStore::open(dir.path().join("execution.sqlite3")).unwrap();
        assert_eq!(
            reopened.assert_reopen_preserves_store_id().unwrap(),
            original_id
        );
        assert_eq!(reopened.snapshot(&snap.run_id).unwrap().run_id, snap.run_id);
    }

    #[test]
    fn writer_serializes_operations() {
        let (store, _dir) = temp_store();
        let writer = ExecutionWriter::spawn(store);
        let snap = writer
            .call(|s| s.create_run("s1", "main", serde_json::json!({}), serde_json::json!({})))
            .unwrap();
        let receipt = writer
            .call(move |s| {
                s.accept_resume("user", &snap.run_id, "r1", 0, None, &serde_json::json!({}))
            })
            .unwrap();
        assert_eq!(receipt.accepted_version, 1);
    }

    #[test]
    fn tool_batch_checkpoint_attempt_and_recovery_are_transactional() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let checkpoints = vec![
            ToolCheckpoint {
                task_id: "safe-task".into(),
                logical_key: "call-safe".into(),
                name: "read".into(),
                call_id: "call-safe".into(),
                arguments: serde_json::json!({ "file_path": "/tmp/x" }),
                policy: crate::agent::tool::RecoveryPolicy::ReplaySafe,
            },
            ToolCheckpoint {
                task_id: "unsafe-task".into(),
                logical_key: "call-unsafe".into(),
                name: "bash".into(),
                call_id: "call-unsafe".into(),
                arguments: serde_json::json!({ "command": "true" }),
                policy: crate::agent::tool::RecoveryPolicy::Unsafe,
            },
        ];
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({ "toolCalls": [] }),
                &checkpoints,
            )
            .unwrap();
        let safe_lease = store
            .start_tool_attempt(&snap.run_id, "safe-task", None)
            .unwrap();
        let unsafe_lease = store
            .start_tool_attempt(&snap.run_id, "unsafe-task", None)
            .unwrap();
        assert_ne!(safe_lease.attempt_id, unsafe_lease.attempt_id);

        let changed = store.recover_interrupted_attempts(&snap.run_id).unwrap();
        assert_eq!(changed, 2);
        let recovered = store.snapshot(&snap.run_id).unwrap();
        assert_eq!(recovered.state, "blocked");
        assert_eq!(
            recovered.reason_code.as_deref(),
            Some("uncertain_tool_effect")
        );
        assert!(
            recovered
                .tasks
                .iter()
                .any(|t| t.task_id == "safe-task" && t.state == "pending")
        );
        assert!(
            recovered
                .tasks
                .iter()
                .any(|t| t.task_id == "unsafe-task" && t.state == "uncertain")
        );
    }

    #[test]
    fn reopen_reconciles_host_epoch_and_uncertain_attempt() {
        let (mut store, dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let checkpoint = ToolCheckpoint {
            task_id: "unsafe-task".into(),
            logical_key: "call-unsafe".into(),
            name: "bash".into(),
            call_id: "call-unsafe".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::Unsafe,
        };
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({}),
                std::slice::from_ref(&checkpoint),
            )
            .unwrap();
        store
            .start_tool_attempt(&snap.run_id, "unsafe-task", Some("old-worker"))
            .unwrap();
        drop(store);

        let mut reopened = ExecutionStore::open(dir.path().join("execution.sqlite3")).unwrap();
        let summary = reopened.reconcile_on_open().unwrap();
        assert_eq!(summary.host_epoch, 1);
        assert_eq!(summary.runs_scanned, 1);
        assert_eq!(summary.changed_attempts, 1);
        let recovered = reopened.snapshot(&snap.run_id).unwrap();
        assert_eq!(recovered.state, "blocked");
        assert_eq!(
            recovered.reason_code.as_deref(),
            Some("uncertain_tool_effect")
        );
    }

    #[test]
    fn fenced_completion_is_idempotent_only_for_active_attempt() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let checkpoint = ToolCheckpoint {
            task_id: "safe-task".into(),
            logical_key: "call-safe".into(),
            name: "read".into(),
            call_id: "call-safe".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::ReplaySafe,
        };
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({}),
                std::slice::from_ref(&checkpoint),
            )
            .unwrap();
        let lease = store
            .start_tool_attempt(&snap.run_id, "safe-task", None)
            .unwrap();
        store
            .complete_tool_attempt(
                &snap.run_id,
                &lease,
                &ToolOutcome {
                    task_id: "safe-task".into(),
                    success: true,
                    output: "ok".into(),
                },
            )
            .unwrap();
        let replay = store.complete_tool_attempt(
            &snap.run_id,
            &lease,
            &ToolOutcome {
                task_id: "safe-task".into(),
                success: true,
                output: "ok".into(),
            },
        );
        assert!(matches!(
            replay.unwrap_err(),
            ExecutionStoreError::Invariant(_)
        ));
        let final_snap = store.snapshot(&snap.run_id).unwrap();
        assert_eq!(final_snap.tasks[0].state, "succeeded");
    }

    #[test]
    fn child_run_checkpoint_is_unique_by_spawn_key() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run(
                "parent",
                "main",
                serde_json::json!({}),
                serde_json::json!({}),
            )
            .unwrap();
        let checkpoint = ToolCheckpoint {
            task_id: "parent-task".into(),
            logical_key: "spawn-parent".into(),
            name: "spawn_worker".into(),
            call_id: "call-parent".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::Unsafe,
        };
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({}),
                std::slice::from_ref(&checkpoint),
            )
            .unwrap();
        let plan = ChildRunPlan {
            parent_task_id: "parent-task".into(),
            spawn_key: "child-a".into(),
            session_id: "child-session".into(),
            input: serde_json::json!({ "task": "do work" }),
        };
        let first = store
            .checkpoint_child_run(&snap.run_id, "parent-task", &plan)
            .unwrap();
        let second = store
            .checkpoint_child_run(&snap.run_id, "parent-task", &plan)
            .unwrap();
        assert_eq!(first, second);
        let snapshot = store.snapshot(&snap.run_id).unwrap();
        assert_eq!(snapshot.state, "waiting_children");
        assert_eq!(snapshot.tasks.len(), 2);
    }

    #[test]
    fn approval_binding_rejects_stale_digest_or_generation() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let checkpoint = ToolCheckpoint {
            task_id: "unsafe-task".into(),
            logical_key: "call".into(),
            name: "bash".into(),
            call_id: "call".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::Unsafe,
        };
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({}),
                std::slice::from_ref(&checkpoint),
            )
            .unwrap();
        let approval = ApprovalCheckpoint {
            approval_id: "apr_1".into(),
            task_id: "unsafe-task".into(),
            action_digest: "digest-1".into(),
            generation: 0,
            policy_version: "v1".into(),
        };
        store.checkpoint_approval(&snap.run_id, &approval).unwrap();
        let stale = store.consume_approval(&snap.run_id, "apr_1", "digest-2", 0);
        assert!(matches!(
            stale.unwrap_err(),
            ExecutionStoreError::Invariant(_)
        ));
    }

    #[test]
    fn child_and_approval_runtime_lifecycle_close_once() {
        let (mut store, _dir) = temp_store();
        let snap = store
            .create_run("s1", "main", serde_json::json!({}), serde_json::json!({}))
            .unwrap();
        let parent = ToolCheckpoint {
            task_id: "parent-task".into(),
            logical_key: "spawn".into(),
            name: "spawn_worker".into(),
            call_id: "call".into(),
            arguments: serde_json::json!({}),
            policy: crate::agent::tool::RecoveryPolicy::Unsafe,
        };
        store
            .checkpoint_tool_batch(
                &snap.run_id,
                &serde_json::json!({}),
                std::slice::from_ref(&parent),
            )
            .unwrap();
        let child = store
            .checkpoint_child_run(
                &snap.run_id,
                "parent-task",
                &ChildRunPlan {
                    parent_task_id: "parent-task".into(),
                    spawn_key: "child".into(),
                    session_id: "child-session".into(),
                    input: serde_json::json!({ "task": "x" }),
                },
            )
            .unwrap();
        store
            .consume_child_input(&snap.run_id, &child, &serde_json::json!({ "ok": true }))
            .unwrap();
        let replay =
            store.consume_child_input(&snap.run_id, &child, &serde_json::json!({ "ok": true }));
        assert!(matches!(
            replay.unwrap_err(),
            ExecutionStoreError::Invariant(_)
        ));

        let approval = ApprovalCheckpoint {
            approval_id: "apr_1".into(),
            task_id: "parent-task".into(),
            action_digest: "digest".into(),
            generation: 0,
            policy_version: "v1".into(),
        };
        store.checkpoint_approval(&snap.run_id, &approval).unwrap();
        store.decide_approval(&snap.run_id, "apr_1", true).unwrap();
        let replay_decision = store.decide_approval(&snap.run_id, "apr_1", true);
        assert!(matches!(
            replay_decision.unwrap_err(),
            ExecutionStoreError::Invariant(_)
        ));
    }

    #[test]
    fn truncated_ledger_is_rejected_without_repair() {
        let (store, dir) = temp_store();
        let path = dir.path().join("execution.sqlite3");
        drop(store);
        let original = std::fs::read(&path).unwrap();
        // Truncate into the SQLite header so this is a malformed database, not
        // merely a benign loss of trailing freelist pages.
        let truncated = if original.len() > 32 {
            original[..32].to_vec()
        } else {
            original
        };
        std::fs::write(&path, truncated).unwrap();
        let opened = ExecutionStore::open(&path);
        assert!(opened.is_err() || opened.and_then(|mut store| store.quick_check()).is_err());
    }
}
