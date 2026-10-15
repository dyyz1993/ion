use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use super::types::{
    EventPage, ExecutionCursor, ResumeReceipt, RunSnapshot, TERMINAL_RUN_STATES, TaskSummary,
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
}

pub type StoreResult<T> = Result<T, ExecutionStoreError>;

pub struct ExecutionStore {
    path: PathBuf,
    conn: Connection,
    store_id: String,
}

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
        let store_id = format!("exec-{}", uuid::Uuid::new_v4());
        Self::initialize(conn, &store_id).map(|conn| Self {
            path: path.as_ref().to_path_buf(),
            conn,
            store_id,
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
            "#,
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO store_meta(singleton, store_id, schema_version, host_epoch) VALUES(1, ?1, ?2, 0)",
            params![store_id, SCHEMA_VERSION],
        )?;
        Ok(conn)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn store_id(&self) -> &str {
        &self.store_id
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
            actions: Default::default(),
        })
    }

    fn load_run(&self, run_id: &str) -> StoreResult<RunRow> {
        self.conn
            .query_row(
                "SELECT run_id,session_id,branch_id,state,reason_code,version,generation,next_event_seq,recovery_plan_id FROM runs WHERE run_id=?1",
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
                let payload = row.get::<_, String>(3)?;
                Ok(serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null))
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
        assert_eq!(
            page.events,
            vec![serde_json::json!({ "runId": snap.run_id })]
        );
        assert_eq!(page.cursor.seq, 1);
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
}
