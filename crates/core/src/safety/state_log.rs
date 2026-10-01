//! Durable journal of pre-mutation system state, backed by SQLite.
//!
//! Every registry value, service configuration, scheduled task enabled flag, Appx package,
//! active power scheme and the DNS servers of each adapter the engine touches is recorded
//! here *before* it is changed, and so is every scheduled task the engine registers (a task
//! definition, whose baseline is "no task at this path"). The first record for a given
//! target is its baseline; later sessions that touch the same target do not overwrite it.
//!
//! A rollback (full, or filtered to selected records; see [`super::rollback`]) restores
//! active records group by group in a fixed order: registry values (then the live settings
//! Windows reads only at sign-in, such as the mouse, are pushed to the running session),
//! services, scheduled tasks, task definitions (deleted), the power scheme, DNS servers, then
//! Appx packages, newest first within each group. Each record that is restored is marked
//! reverted. Records that fail to restore, and packages that need a Microsoft Store
//! reinstall, stay active so a later rollback retries them.
//!
//! The journal also keeps the results of scheduled maintenance runs (`maintenance_runs`);
//! they describe what happened and are never rolled back.
//!
//! [`Journal::open`] reads the schema version before it writes anything: a journal written
//! by a newer schema is refused with [`Error::JournalTooNew`] and left unchanged, and the
//! stored version is never lowered.

use std::path::{Path, PathBuf};

use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::config::DbConfig;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::network::{self, IpFamily};
use crate::win::registry::{path_within, Hive, RawValue, RegValue};
use crate::win::scm::StartType;
use crate::{Error, Result};

/// Version of the schema this build writes (`PRAGMA user_version`); journals with a higher
/// version are refused.
pub const SCHEMA_VERSION: i64 = 5;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id                INTEGER PRIMARY KEY,
    started_at        TEXT    NOT NULL,
    ended_at          TEXT,
    label             TEXT    NOT NULL,
    tool_version      TEXT    NOT NULL,
    restore_point_seq INTEGER
);

CREATE TABLE IF NOT EXISTS registry_journal (
    id            INTEGER PRIMARY KEY,
    session_id    INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at   TEXT    NOT NULL,
    hive          TEXT    NOT NULL,
    key_path      TEXT    NOT NULL,
    value_name    TEXT    NOT NULL,
    key_existed   INTEGER NOT NULL,
    value_existed INTEGER NOT NULL,
    value_type    INTEGER,
    value_data    BLOB,
    active        INTEGER NOT NULL DEFAULT 1,
    reverted_at   TEXT,
    -- Shallowest key the write created when key_existed = 0 (the leaf or one of its
    -- ancestors); NULL when the key existed or for rows written before schema 3.
    created_root  TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_registry_active
    ON registry_journal (hive, key_path COLLATE NOCASE, value_name COLLATE NOCASE)
    WHERE active = 1;

CREATE TABLE IF NOT EXISTS service_journal (
    id                 INTEGER PRIMARY KEY,
    session_id         INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at        TEXT    NOT NULL,
    name               TEXT    NOT NULL,
    display_name       TEXT    NOT NULL,
    start_type         TEXT    NOT NULL,
    delayed_auto_start INTEGER NOT NULL,
    was_running        INTEGER NOT NULL,
    active             INTEGER NOT NULL DEFAULT 1,
    reverted_at        TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_service_active
    ON service_journal (name COLLATE NOCASE)
    WHERE active = 1;

CREATE TABLE IF NOT EXISTS appx_journal (
    id                INTEGER PRIMARY KEY,
    session_id        INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at       TEXT    NOT NULL,
    package_full_name TEXT    NOT NULL,
    package_family    TEXT    NOT NULL,
    install_location  TEXT    NOT NULL,
    all_users         INTEGER NOT NULL,
    active            INTEGER NOT NULL DEFAULT 1,
    reverted_at       TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_appx_active
    ON appx_journal (package_full_name COLLATE NOCASE)
    WHERE active = 1;

-- At most one active record: the scheme that was active before the first change.
CREATE TABLE IF NOT EXISTS power_journal (
    id              INTEGER PRIMARY KEY,
    session_id      INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at     TEXT    NOT NULL,
    slot            INTEGER NOT NULL DEFAULT 0,
    previous_scheme TEXT    NOT NULL,
    target_scheme   TEXT    NOT NULL,
    active          INTEGER NOT NULL DEFAULT 1,
    reverted_at     TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_power_active
    ON power_journal (slot)
    WHERE active = 1;

-- Enabled flag of a scheduled task before the engine first changed it. `path` is the
-- task's full path exactly as the catalog names it.
CREATE TABLE IF NOT EXISTS scheduled_task_journal (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at TEXT    NOT NULL,
    path        TEXT    NOT NULL,
    was_enabled INTEGER NOT NULL,
    active      INTEGER NOT NULL DEFAULT 1,
    reverted_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_scheduled_task_active
    ON scheduled_task_journal (path COLLATE NOCASE) WHERE active = 1;

-- Static DNS servers of one interface and address family before the first change,
-- stored verbatim ('' = obtained automatically). `interface_guid` is canonical: '{lowercase}'.
-- `target_servers` holds the servers the engine last wrote while the record is active.
CREATE TABLE IF NOT EXISTS dns_journal (
    id               INTEGER PRIMARY KEY,
    session_id       INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at      TEXT    NOT NULL,
    interface_guid   TEXT    NOT NULL,
    family           TEXT    NOT NULL,
    adapter_name     TEXT    NOT NULL,
    previous_servers TEXT    NOT NULL,
    target_servers   TEXT    NOT NULL,
    active           INTEGER NOT NULL DEFAULT 1,
    reverted_at      TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_dns_active
    ON dns_journal (interface_guid COLLATE NOCASE, family) WHERE active = 1;

-- Task Scheduler tasks the engine registered. Every record's baseline is "no task at `path`": a
-- rollback deletes the task, and removes its folder once empty when `folder_created` = 1.
CREATE TABLE IF NOT EXISTS task_definition_journal (
    id             INTEGER PRIMARY KEY,
    session_id     INTEGER NOT NULL REFERENCES sessions(id),
    recorded_at    TEXT    NOT NULL,
    path           TEXT    NOT NULL,
    purpose        TEXT    NOT NULL,
    folder_created INTEGER NOT NULL,
    active         INTEGER NOT NULL DEFAULT 1,
    reverted_at    TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS ux_task_definition_active
    ON task_definition_journal (path COLLATE NOCASE) WHERE active = 1;

-- Scheduled maintenance runs: status and results; never rolled back.
CREATE TABLE IF NOT EXISTS maintenance_runs (
    id              INTEGER PRIMARY KEY,
    started_at      TEXT    NOT NULL,
    ended_at        TEXT,
    state           TEXT    NOT NULL,
    origin          TEXT    NOT NULL,
    request         TEXT    NOT NULL,
    progress        TEXT,
    report          TEXT,
    log_path        TEXT,
    acknowledged_at TEXT
);
CREATE INDEX IF NOT EXISTS ix_maintenance_runs_state ON maintenance_runs (state);

CREATE TABLE IF NOT EXISTS ops_log (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER REFERENCES sessions(id),
    ts         TEXT NOT NULL,
    op         TEXT NOT NULL,
    target     TEXT NOT NULL,
    outcome    TEXT NOT NULL,
    detail     TEXT
);
"#;

/// Per-user data folder and parent of [`default_path`]: `%OPTIMIZER_DATA_DIR%`, else
/// `%LOCALAPPDATA%\PCOptimizer`.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPTIMIZER_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("PCOptimizer")
}

/// `%OPTIMIZER_DATA_DIR%\journal.db`, else `%LOCALAPPDATA%\PCOptimizer\journal.db`.
pub fn default_path() -> PathBuf {
    data_dir().join("journal.db")
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// Columns added after a table's first release, as `(table, column, definition)`.
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table unchanged, so a journal created
/// by an older version gets them through `ALTER TABLE`.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[("registry_journal", "created_root", "TEXT")];

fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2 COLLATE NOCASE")?
        .exists(params![table, column])
}

/// Adds every column of [`ADDED_COLUMNS`] the database lacks. The check is repeated
/// under a write lock so two processes opening an old journal at once add it only once.
fn add_missing_columns(conn: &mut Connection) -> Result<()> {
    for &(table, column, definition) in ADDED_COLUMNS {
        if has_column(conn, table, column)? {
            continue;
        }
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !has_column(&tx, table, column)? {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition}"
            ))?;
        }
        tx.commit()?;
    }
    Ok(())
}

/// The file SQLite keeps beside the database `path` (`-wal`, `-shm`).
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Raises `user_version` to [`SCHEMA_VERSION`]. The version is re-read under a write lock, so
/// a newer version another process stored in the meantime is kept.
fn stamp_schema_version(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if current < SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}

// ───────────────────────────── Records ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: i64,
    pub label: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub tool_version: String,
    pub restore_point_seq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewRegistryRecord {
    pub hive: Hive,
    pub key_path: String,
    pub value_name: String,
    pub key_existed: bool,
    pub value_existed: bool,
    pub original: Option<RawValue>,
    /// When the key did not exist: the shallowest missing key on its path, i.e. the
    /// topmost key that writing the value creates (`key_path` itself when only the leaf
    /// is missing). `None` when the key existed.
    #[serde(default)]
    pub created_root: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub hive: Hive,
    pub key_path: String,
    pub value_name: String,
    pub key_existed: bool,
    pub value_existed: bool,
    pub original: Option<RawValue>,
    pub active: bool,
    pub reverted_at: Option<String>,
    /// See [`NewRegistryRecord::created_root`]. Always `None` for records written before
    /// the column existed; rollback then treats only `key_path` as created.
    #[serde(default)]
    pub created_root: Option<String>,
}

impl RegistryRecord {
    pub fn target(&self) -> String {
        let name = if self.value_name.is_empty() {
            "(Default)"
        } else {
            &self.value_name
        };
        format!("{}\\{}\\{}", self.hive.short(), self.key_path, name)
    }

    pub fn original_decoded(&self) -> Option<RegValue> {
        self.original.as_ref().map(RawValue::decode)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewServiceRecord {
    pub name: String,
    pub display_name: String,
    pub start_type: StartType,
    pub delayed_auto_start: bool,
    pub was_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub name: String,
    pub display_name: String,
    pub start_type: StartType,
    pub delayed_auto_start: bool,
    pub was_running: bool,
    pub active: bool,
    pub reverted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewAppxRecord {
    pub package_full_name: String,
    pub package_family: String,
    pub install_location: String,
    pub all_users: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppxRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub package_full_name: String,
    pub package_family: String,
    pub install_location: String,
    pub all_users: bool,
    pub active: bool,
    pub reverted_at: Option<String>,
}

/// Active power scheme before the engine changed it. Scheme identifiers are lowercase
/// GUID strings without braces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewPowerRecord {
    pub previous_scheme: String,
    pub target_scheme: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub previous_scheme: String,
    pub target_scheme: String,
    pub active: bool,
    pub reverted_at: Option<String>,
}

/// Target string of a scheduled task in actions, logs and the catalog:
/// `scheduled task <path>`.
pub fn scheduled_task_target(path: &str) -> String {
    format!("scheduled task {path}")
}

/// Enabled flag of a scheduled task before the engine changed it. `path` is the task's full
/// path as the catalog names it (for example `\Microsoft\Windows\Autochk\Proxy`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewScheduledTaskRecord {
    pub path: String,
    pub was_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTaskRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub path: String,
    pub was_enabled: bool,
    pub active: bool,
    pub reverted_at: Option<String>,
}

impl ScheduledTaskRecord {
    pub fn target(&self) -> String {
        scheduled_task_target(&self.path)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScheduledTaskExportEntry {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub target: String,
    pub path: String,
    pub was_enabled: bool,
    pub active: bool,
    pub reverted_at: Option<String>,
}

/// Static DNS servers of one interface and address family before the engine changed them,
/// and the servers it writes. Server lists are stored verbatim as Windows reports them; `""`
/// means the servers are obtained automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDnsRecord {
    pub interface_guid: String,
    pub family: IpFamily,
    pub adapter_name: String,
    pub previous_servers: String,
    pub target_servers: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DnsRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    /// Canonical interface GUID: `{lowercase}`.
    pub interface_guid: String,
    pub family: IpFamily,
    pub adapter_name: String,
    /// The baseline: the servers before the engine's first change.
    pub previous_servers: String,
    /// The servers the engine last wrote; later changes keep the baseline and update this.
    pub target_servers: String,
    pub active: bool,
    pub reverted_at: Option<String>,
}

impl DnsRecord {
    /// For example `IPv4 DNS servers of Wi-Fi`.
    pub fn target(&self) -> String {
        network::dns_target(self.family, &self.adapter_name)
    }

    /// The recorded servers as text: `automatic` or the servers joined with `, `.
    pub fn previous_text(&self) -> String {
        network::servers_text(&self.previous_servers)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DnsExportEntry {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub target: String,
    pub interface_guid: String,
    pub family: IpFamily,
    pub adapter_name: String,
    /// Empty when the servers were obtained automatically.
    pub previous_servers: Vec<String>,
    pub target_servers: Vec<String>,
    pub active: bool,
    pub reverted_at: Option<String>,
}

/// Target string of a task definition in actions, logs and History: `task <path>`.
pub fn task_definition_target(path: &str) -> String {
    format!("task {path}")
}

/// A Task Scheduler task the engine registers. The baseline is always "no task at `path`";
/// `folder_created` is true when the engine also created the task's folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTaskDefinitionRecord {
    /// Full task path, for example `\Cairn\Maintenance-S-1-5-21-…`.
    pub path: String,
    /// What the task is for, for example `maintenance`.
    pub purpose: String,
    pub folder_created: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskDefinitionRecord {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub path: String,
    pub purpose: String,
    pub folder_created: bool,
    pub active: bool,
    pub reverted_at: Option<String>,
}

impl TaskDefinitionRecord {
    pub fn target(&self) -> String {
        task_definition_target(&self.path)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDefinitionExportEntry {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub target: String,
    pub path: String,
    pub purpose: String,
    pub folder_created: bool,
    pub active: bool,
    pub reverted_at: Option<String>,
}

/// A scheduled maintenance run as it starts. `state` is normally `running`; `request_json` is
/// the serialized request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMaintenanceRun {
    pub origin: String,
    pub state: String,
    pub request_json: String,
}

/// One row of `maintenance_runs`. Times are RFC 3339 UTC; the JSON columns are kept as text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceRunRow {
    pub id: i64,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub state: String,
    pub origin: String,
    pub request_json: String,
    pub progress_json: Option<String>,
    pub report_json: Option<String>,
    pub log_path: Option<String>,
    pub acknowledged_at: Option<String>,
}

/// State of a maintenance run that has not finished.
const RUN_RUNNING: &str = "running";
/// State of a maintenance run that ended without finishing.
const RUN_INTERRUPTED: &str = "interrupted";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalTable {
    Registry,
    Service,
    Appx,
    Power,
    ScheduledTask,
    Dns,
    TaskDefinition,
}

impl JournalTable {
    fn name(self) -> &'static str {
        match self {
            JournalTable::Registry => "registry_journal",
            JournalTable::Service => "service_journal",
            JournalTable::Appx => "appx_journal",
            JournalTable::Power => "power_journal",
            JournalTable::ScheduledTask => "scheduled_task_journal",
            JournalTable::Dns => "dns_journal",
            JournalTable::TaskDefinition => "task_definition_journal",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpLogEntry {
    pub id: i64,
    pub session_id: Option<i64>,
    pub ts: String,
    pub op: String,
    pub target: String,
    pub outcome: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalSummary {
    pub path: String,
    pub sessions: i64,
    pub registry_active: i64,
    pub registry_total: i64,
    pub services_active: i64,
    pub services_total: i64,
    #[serde(default)]
    pub scheduled_tasks_active: i64,
    #[serde(default)]
    pub scheduled_tasks_total: i64,
    pub appx_active: i64,
    pub appx_total: i64,
    pub power_active: i64,
    pub power_total: i64,
    #[serde(default)]
    pub dns_active: i64,
    #[serde(default)]
    pub dns_total: i64,
    #[serde(default)]
    pub task_definitions_active: i64,
    #[serde(default)]
    pub task_definitions_total: i64,
    pub last_session: Option<SessionInfo>,
}

impl JournalSummary {
    /// Active records of every kind: the changes a Revert All would undo.
    pub fn pending_count(&self) -> i64 {
        self.registry_active
            + self.services_active
            + self.scheduled_tasks_active
            + self.appx_active
            + self.power_active
            + self.dns_active
            + self.task_definitions_active
    }

    pub fn has_pending_changes(&self) -> bool {
        self.pending_count() > 0
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryExportEntry {
    pub id: i64,
    pub session_id: i64,
    pub recorded_at: String,
    pub target: String,
    pub hive: Hive,
    pub key_path: String,
    pub value_name: String,
    pub key_existed: bool,
    pub value_existed: bool,
    pub original: Option<RegValue>,
    pub active: bool,
    pub reverted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JournalExport {
    pub exported_at: String,
    pub summary: JournalSummary,
    pub sessions: Vec<SessionInfo>,
    pub registry: Vec<RegistryExportEntry>,
    pub services: Vec<ServiceRecord>,
    pub scheduled_tasks: Vec<ScheduledTaskExportEntry>,
    pub appx: Vec<AppxRecord>,
    pub power: Vec<PowerRecord>,
    pub dns: Vec<DnsExportEntry>,
    pub task_definitions: Vec<TaskDefinitionExportEntry>,
    pub ops: Vec<OpLogEntry>,
}

// ───────────────────────────── created keys ─────────────────────────────

/// A registry record's key and the time its value was under the engine's control, as
/// [`Journal::created_keys_before`] reads it. Times are RFC 3339 UTC strings from
/// [`now`], which order the same way as text and as instants.
#[derive(Debug)]
struct KeySpan {
    id: i64,
    key_path: String,
    /// Shallowest key the write created (`key_path` for rows without one); meaningful
    /// only when `created`.
    root: String,
    /// The key did not exist when the record was written.
    created: bool,
    recorded_at: String,
    reverted_at: Option<String>,
}

/// `path` and its parents up to and including `root`, deepest first; `path` alone when
/// it does not lie below `root`.
fn created_chain<'a>(path: &'a str, root: &'a str) -> impl Iterator<Item = &'a str> {
    let shallowest = if path_within(path, root) {
        root.len()
    } else {
        path.len()
    };
    std::iter::successors(Some(path), move |key| {
        key.rsplit_once('\\')
            .map(|(parent, _)| parent)
            .filter(|parent| parent.len() >= shallowest)
    })
}

/// True when, at every moment from `creator`'s revert until `until`, at least one record in
/// `holders` whose key lies at or below `key` was active; the records may take turns. Each
/// such record kept a value below `key`, so the key was never empty and no rollback removed
/// it in that time.
fn held_until(creator: &KeySpan, holders: &[&KeySpan], key: &str, until: &str) -> bool {
    let Some(mut covered) = creator.reverted_at.as_deref() else {
        return true;
    };
    while covered < until {
        let mut next: Option<&str> = None;
        for span in holders
            .iter()
            .filter(|s| s.recorded_at.as_str() <= covered && path_within(&s.key_path, key))
        {
            match span.reverted_at.as_deref() {
                None => return true,
                Some(end) if end > covered => next = Some(next.map_or(end, |n| n.max(end))),
                Some(_) => {}
            }
        }
        match next {
            Some(end) => covered = end,
            None => return false,
        }
    }
    true
}

// ───────────────────────────── Journal ─────────────────────────────

#[derive(Debug)]
pub struct Journal {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl Journal {
    /// Opens the journal at `path`, creating it (and its folder) when missing.
    ///
    /// The stored schema version is read before anything is written: a journal written by a
    /// newer schema is refused with [`Error::JournalTooNew`] and left unchanged. An older one
    /// gains the missing tables and columns and is stamped with [`SCHEMA_VERSION`]; the
    /// version is never lowered.
    pub fn open(path: impl AsRef<Path>) -> Result<Journal> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // The last connection to a WAL database checkpoints the WAL into the main file when it
        // closes. A WAL that exists before the version is known stays as it is until the
        // version is accepted, so a refused journal keeps every byte of both files. Without a
        // WAL there is nothing to checkpoint, and closing removes the files reading created.
        let keep_wal = sidecar(&path, "-wal").exists();
        let mut conn = Connection::open(&path)?;
        if keep_wal {
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
        }
        let found: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > SCHEMA_VERSION {
            drop(conn);
            return Err(Error::JournalTooNew {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        if keep_wal {
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, false)?;
        }
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        add_missing_columns(&mut conn)?;
        if found < SCHEMA_VERSION {
            stamp_schema_version(&mut conn)?;
        }
        Ok(Journal {
            conn: Mutex::new(conn),
            path,
        })
    }

    pub fn open_default() -> Result<Journal> {
        Journal::open(default_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    // ── sessions ──

    pub fn begin_session(&self, label: &str, tool_version: &str) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO sessions (started_at, label, tool_version) VALUES (?1, ?2, ?3)",
            params![now(), label, tool_version],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn end_session(&self, id: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2 AND ended_at IS NULL",
            params![now(), id],
        )?;
        Ok(())
    }

    pub fn set_session_restore_point(&self, id: i64, sequence: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE sessions SET restore_point_seq = ?1 WHERE id = ?2",
            params![sequence, id],
        )?;
        Ok(())
    }

    pub fn sessions(&self) -> Result<Vec<SessionInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, label, started_at, ended_at, tool_version, restore_point_seq
             FROM sessions ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SessionInfo {
                id: r.get(0)?,
                label: r.get(1)?,
                started_at: r.get(2)?,
                ended_at: r.get(3)?,
                tool_version: r.get(4)?,
                restore_point_seq: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── recording (baseline capture) ──

    /// Records the pre-change state of a registry value. Returns `true` when this call
    /// captured the baseline, `false` when an active baseline already existed.
    pub fn record_registry(&self, session_id: i64, rec: &NewRegistryRecord) -> Result<bool> {
        let (kind, data) = match &rec.original {
            Some(raw) => (Some(raw.kind as i64), Some(raw.data.clone())),
            None => (None, None),
        };
        let conn = self.conn.lock();
        let created_root = if rec.key_existed {
            None
        } else {
            rec.created_root.as_deref()
        };
        let n = conn.execute(
            "INSERT OR IGNORE INTO registry_journal
             (session_id, recorded_at, hive, key_path, value_name, key_existed, value_existed,
              value_type, value_data, created_root)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session_id,
                now(),
                rec.hive.short(),
                rec.key_path,
                rec.value_name,
                rec.key_existed,
                rec.value_existed,
                kind,
                data,
                created_root
            ],
        )?;
        Ok(n == 1)
    }

    pub fn record_service(&self, session_id: i64, rec: &NewServiceRecord) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO service_journal
             (session_id, recorded_at, name, display_name, start_type, delayed_auto_start, was_running)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                now(),
                rec.name,
                rec.display_name,
                rec.start_type.label(),
                rec.delayed_auto_start,
                rec.was_running
            ],
        )?;
        Ok(n == 1)
    }

    pub fn record_appx(&self, session_id: i64, rec: &NewAppxRecord) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO appx_journal
             (session_id, recorded_at, package_full_name, package_family, install_location, all_users)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id,
                now(),
                rec.package_full_name,
                rec.package_family,
                rec.install_location,
                rec.all_users
            ],
        )?;
        Ok(n == 1)
    }

    /// Records the scheme that was active before the engine's first power change. Later
    /// changes keep that baseline while it is active.
    pub fn record_power(&self, session_id: i64, rec: &NewPowerRecord) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO power_journal
             (session_id, recorded_at, previous_scheme, target_scheme)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session_id,
                now(),
                rec.previous_scheme.to_ascii_lowercase(),
                rec.target_scheme.to_ascii_lowercase()
            ],
        )?;
        Ok(n == 1)
    }

    /// Records a scheduled task's enabled flag before the engine first changes it. The
    /// path is stored as given; an active baseline for the same path (compared ignoring
    /// case) is kept. Returns `true` when this call captured the baseline.
    pub fn record_scheduled_task(
        &self,
        session_id: i64,
        rec: &NewScheduledTaskRecord,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO scheduled_task_journal
             (session_id, recorded_at, path, was_enabled)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, now(), rec.path, rec.was_enabled],
        )?;
        Ok(n == 1)
    }

    /// Records one interface's static DNS servers of one address family before the engine
    /// first changes them. The interface GUID is stored in canonical form, so braced and
    /// bare spellings of one GUID share a baseline. Returns `true` when this call captured
    /// the baseline; an active baseline keeps its servers and its target (see
    /// [`Self::update_dns_target`]).
    pub fn record_dns(&self, session_id: i64, rec: &NewDnsRecord) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO dns_journal
             (session_id, recorded_at, interface_guid, family, adapter_name, previous_servers,
              target_servers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                now(),
                network::canonical_guid(&rec.interface_guid),
                rec.family.as_str(),
                rec.adapter_name,
                rec.previous_servers,
                rec.target_servers
            ],
        )?;
        Ok(n == 1)
    }

    /// Sets the target of the active DNS record of `interface_guid` (any spelling) and
    /// `family` to `target_servers`, after the engine wrote them over an older baseline. The
    /// baseline itself is kept. Returns whether an active record was updated.
    pub fn update_dns_target(
        &self,
        interface_guid: &str,
        family: IpFamily,
        target_servers: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE dns_journal SET target_servers = ?3
             WHERE active = 1 AND interface_guid = ?1 COLLATE NOCASE AND family = ?2",
            params![
                network::canonical_guid(interface_guid),
                family.as_str(),
                target_servers
            ],
        )?;
        Ok(n > 0)
    }

    /// Records a Task Scheduler task before the engine registers it. The path is stored as
    /// given; an active record for the same path (compared ignoring case) is kept, with its
    /// `folder_created`. Returns `true` when this call captured the baseline.
    pub fn record_task_definition(
        &self,
        session_id: i64,
        rec: &NewTaskDefinitionRecord,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "INSERT OR IGNORE INTO task_definition_journal
             (session_id, recorded_at, path, purpose, folder_created)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, now(), rec.path, rec.purpose, rec.folder_created],
        )?;
        Ok(n == 1)
    }

    // ── reading ──

    pub fn active_registry(&self) -> Result<Vec<RegistryRecord>> {
        self.registry_where("WHERE active = 1")
    }

    pub fn all_registry(&self) -> Result<Vec<RegistryRecord>> {
        self.registry_where("")
    }

    fn registry_where(&self, filter: &str) -> Result<Vec<RegistryRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, hive, key_path, value_name, key_existed,
                    value_existed, value_type, value_data, active, reverted_at, created_root
             FROM registry_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            let hive_text: String = r.get(3)?;
            let hive = Hive::parse(&hive_text).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown hive {hive_text}"),
                    )),
                )
            })?;
            let value_existed: bool = r.get(7)?;
            let value_type: Option<i64> = r.get(8)?;
            let value_data: Option<Vec<u8>> = r.get(9)?;
            let original = if value_existed {
                Some(RawValue {
                    kind: value_type.unwrap_or(0) as u32,
                    data: value_data.unwrap_or_default(),
                })
            } else {
                None
            };
            Ok(RegistryRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                hive,
                key_path: r.get(4)?,
                value_name: r.get(5)?,
                key_existed: r.get(6)?,
                value_existed,
                original,
                active: r.get(10)?,
                reverted_at: r.get(11)?,
                created_root: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Keys on the path to `rec`'s key that older engine writes created and that `rec` may
    /// still depend on, as `(key_path, created_root)` pairs in record order: every key from
    /// `key_path` up to `created_root` counts as created. Each older record (smaller id)
    /// whose key did not exist, and whose `created_root` contains `rec`'s key, adds a pair.
    /// Records written before `created_root` was journaled report `key_path` as their root.
    ///
    /// A creating record that was still active when `rec` was written adds its own
    /// `key_path`. One reverted before that adds a pair only for the keys that outlived the
    /// revert: those below which, at every moment from the revert until `rec` was written,
    /// at least one other journaled record was active (records may take turns), so they
    /// were never empty in between. Its pair
    /// then starts at the deepest such key. A key left without any journaled value in
    /// between may have been removed, so whatever `rec` found there was not created by
    /// the engine and is left out.
    pub fn created_keys_before(&self, rec: &RegistryRecord) -> Result<Vec<(String, String)>> {
        let spans = self.key_spans(rec.hive)?;
        let written = rec.recorded_at.as_str();
        let mut claims = Vec::new();
        for creator in spans
            .iter()
            .filter(|s| s.created && s.id < rec.id && path_within(&rec.key_path, &s.root))
        {
            let still_active = match creator.reverted_at.as_deref() {
                None => true,
                Some(t) => t >= written,
            };
            let kept = if still_active {
                Some(creator.key_path.as_str())
            } else {
                let holders: Vec<&KeySpan> = spans
                    .iter()
                    .filter(|s| s.id != creator.id && path_within(&s.key_path, &creator.root))
                    .collect();
                created_chain(&creator.key_path, &creator.root)
                    .find(|key| held_until(creator, &holders, key, written))
            };
            if let Some(key) = kept {
                claims.push((key.to_string(), creator.root.clone()));
            }
        }
        Ok(claims)
    }

    /// Every registry record of `hive` in id order, reduced to its key and lifetime.
    fn key_spans(&self, hive: Hive) -> Result<Vec<KeySpan>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, key_path, COALESCE(created_root, key_path), key_existed,
                    recorded_at, reverted_at
             FROM registry_journal WHERE hive = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![hive.short()], |r| {
            let key_path: String = r.get(1)?;
            let root: String = r.get(2)?;
            let key_existed: bool = r.get(3)?;
            // A root that does not contain the key (a damaged row) claims the key alone.
            let root = if path_within(&key_path, &root) {
                root
            } else {
                key_path.clone()
            };
            Ok(KeySpan {
                id: r.get(0)?,
                key_path,
                root,
                created: !key_existed,
                recorded_at: r.get(4)?,
                reverted_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_services(&self) -> Result<Vec<ServiceRecord>> {
        self.services_where("WHERE active = 1")
    }

    pub fn all_services(&self) -> Result<Vec<ServiceRecord>> {
        self.services_where("")
    }

    fn services_where(&self, filter: &str) -> Result<Vec<ServiceRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, name, display_name, start_type,
                    delayed_auto_start, was_running, active, reverted_at
             FROM service_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            let start_text: String = r.get(5)?;
            let start_type = StartType::parse(&start_text).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    5,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown start type {start_text}"),
                    )),
                )
            })?;
            Ok(ServiceRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                name: r.get(3)?,
                display_name: r.get(4)?,
                start_type,
                delayed_auto_start: r.get(6)?,
                was_running: r.get(7)?,
                active: r.get(8)?,
                reverted_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_appx(&self) -> Result<Vec<AppxRecord>> {
        self.appx_where("WHERE active = 1")
    }

    pub fn all_appx(&self) -> Result<Vec<AppxRecord>> {
        self.appx_where("")
    }

    fn appx_where(&self, filter: &str) -> Result<Vec<AppxRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, package_full_name, package_family,
                    install_location, all_users, active, reverted_at
             FROM appx_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(AppxRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                package_full_name: r.get(3)?,
                package_family: r.get(4)?,
                install_location: r.get(5)?,
                all_users: r.get(6)?,
                active: r.get(7)?,
                reverted_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_power(&self) -> Result<Vec<PowerRecord>> {
        self.power_where("WHERE active = 1")
    }

    pub fn all_power(&self) -> Result<Vec<PowerRecord>> {
        self.power_where("")
    }

    fn power_where(&self, filter: &str) -> Result<Vec<PowerRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, previous_scheme, target_scheme, active, reverted_at
             FROM power_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(PowerRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                previous_scheme: r.get(3)?,
                target_scheme: r.get(4)?,
                active: r.get(5)?,
                reverted_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_scheduled_tasks(&self) -> Result<Vec<ScheduledTaskRecord>> {
        self.scheduled_tasks_where("WHERE active = 1")
    }

    pub fn all_scheduled_tasks(&self) -> Result<Vec<ScheduledTaskRecord>> {
        self.scheduled_tasks_where("")
    }

    fn scheduled_tasks_where(&self, filter: &str) -> Result<Vec<ScheduledTaskRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, path, was_enabled, active, reverted_at
             FROM scheduled_task_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(ScheduledTaskRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                path: r.get(3)?,
                was_enabled: r.get(4)?,
                active: r.get(5)?,
                reverted_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_dns(&self) -> Result<Vec<DnsRecord>> {
        self.dns_where("WHERE active = 1")
    }

    pub fn all_dns(&self) -> Result<Vec<DnsRecord>> {
        self.dns_where("")
    }

    fn dns_where(&self, filter: &str) -> Result<Vec<DnsRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, interface_guid, family, adapter_name,
                    previous_servers, target_servers, active, reverted_at
             FROM dns_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            let family_text: String = r.get(4)?;
            let family = IpFamily::parse(&family_text).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unknown address family {family_text}"),
                    )),
                )
            })?;
            Ok(DnsRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                interface_guid: r.get(3)?,
                family,
                adapter_name: r.get(5)?,
                previous_servers: r.get(6)?,
                target_servers: r.get(7)?,
                active: r.get(8)?,
                reverted_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn active_task_definitions(&self) -> Result<Vec<TaskDefinitionRecord>> {
        self.task_definitions_where("WHERE active = 1")
    }

    pub fn all_task_definitions(&self) -> Result<Vec<TaskDefinitionRecord>> {
        self.task_definitions_where("")
    }

    fn task_definitions_where(&self, filter: &str) -> Result<Vec<TaskDefinitionRecord>> {
        let conn = self.conn.lock();
        let sql = format!(
            "SELECT id, session_id, recorded_at, path, purpose, folder_created, active, reverted_at
             FROM task_definition_journal {filter} ORDER BY id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(TaskDefinitionRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                recorded_at: r.get(2)?,
                path: r.get(3)?,
                purpose: r.get(4)?,
                folder_created: r.get(5)?,
                active: r.get(6)?,
                reverted_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── maintenance runs (results only; never rolled back) ──

    /// Inserts a run row started now and returns its id.
    pub fn insert_maintenance_run(&self, run: &NewMaintenanceRun) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO maintenance_runs (started_at, state, origin, request)
             VALUES (?1, ?2, ?3, ?4)",
            params![now(), run.state, run.origin, run.request_json],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Replaces the progress JSON of run `id`.
    pub fn update_maintenance_progress(&self, id: i64, progress_json: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE maintenance_runs SET progress = ?2 WHERE id = ?1",
            params![id, progress_json],
        )?;
        Ok(())
    }

    /// Stores the final state and report of run `id` and ends it now. A `None` log path
    /// keeps the one stored earlier.
    pub fn finish_maintenance_run(
        &self,
        id: i64,
        state: &str,
        report_json: &str,
        log_path: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE maintenance_runs
             SET state = ?2, ended_at = ?3, report = ?4, log_path = COALESCE(?5, log_path)
             WHERE id = ?1",
            params![id, state, now(), report_json, log_path],
        )?;
        Ok(())
    }

    /// Marks every run still in state `running` as `interrupted`, ended now, and returns
    /// their ids (oldest first). Called by a new run, which holds the run lock, so any run
    /// left `running` ended without finishing.
    pub fn interrupt_running_maintenance_runs(&self) -> Result<Vec<i64>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ids = {
            let mut stmt =
                tx.prepare("SELECT id FROM maintenance_runs WHERE state = ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![RUN_RUNNING], |r| r.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute(
            "UPDATE maintenance_runs SET state = ?2, ended_at = ?3 WHERE state = ?1",
            params![RUN_RUNNING, RUN_INTERRUPTED, now()],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    /// Marks run `id` as `interrupted`, ended now, when it is still in state `running`, and
    /// returns whether it was. The check and the change are one statement, so a run another
    /// process closed in the meantime is not closed twice; no other row is touched.
    pub fn interrupt_maintenance_run(&self, id: i64) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE maintenance_runs SET state = ?2, ended_at = ?3
             WHERE id = ?1 AND state = ?4",
            params![id, RUN_INTERRUPTED, now(), RUN_RUNNING],
        )?;
        Ok(n > 0)
    }

    /// The newest `limit` runs, newest first.
    pub fn maintenance_runs(&self, limit: usize) -> Result<Vec<MaintenanceRunRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, started_at, ended_at, state, origin, request, progress, report, log_path,
                    acknowledged_at
             FROM maintenance_runs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(MaintenanceRunRow {
                id: r.get(0)?,
                started_at: r.get(1)?,
                ended_at: r.get(2)?,
                state: r.get(3)?,
                origin: r.get(4)?,
                request_json: r.get(5)?,
                progress_json: r.get(6)?,
                report_json: r.get(7)?,
                log_path: r.get(8)?,
                acknowledged_at: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Marks finished run `id` as seen. Returns false when the run does not exist, is still
    /// running or was acknowledged before.
    pub fn acknowledge_maintenance_run(&self, id: i64) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE maintenance_runs SET acknowledged_at = ?2
             WHERE id = ?1 AND acknowledged_at IS NULL AND state <> ?3",
            params![id, now(), RUN_RUNNING],
        )?;
        Ok(n > 0)
    }

    /// Deletes every run older than the newest `keep`, except runs still in state `running`.
    /// Returns the number of rows deleted.
    pub fn prune_maintenance_runs(&self, keep: usize) -> Result<usize> {
        let n = self.conn.lock().execute(
            "DELETE FROM maintenance_runs
             WHERE state <> ?2
               AND id NOT IN (SELECT id FROM maintenance_runs ORDER BY id DESC LIMIT ?1)",
            params![keep as i64, RUN_RUNNING],
        )?;
        Ok(n)
    }

    pub fn ops(&self, limit: usize) -> Result<Vec<OpLogEntry>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, session_id, ts, op, target, outcome, detail
             FROM ops_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok(OpLogEntry {
                id: r.get(0)?,
                session_id: r.get(1)?,
                ts: r.get(2)?,
                op: r.get(3)?,
                target: r.get(4)?,
                outcome: r.get(5)?,
                detail: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ── mutation bookkeeping ──

    pub fn mark_reverted(&self, table: JournalTable, id: i64) -> Result<()> {
        let sql = format!(
            "UPDATE {} SET active = 0, reverted_at = ?1 WHERE id = ?2 AND active = 1",
            table.name()
        );
        self.conn.lock().execute(&sql, params![now(), id])?;
        Ok(())
    }

    pub fn log_op(
        &self,
        session_id: Option<i64>,
        op: &str,
        target: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO ops_log (session_id, ts, op, target, outcome, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session_id, now(), op, target, outcome, detail],
        )?;
        Ok(())
    }

    // ── reporting ──

    pub fn summary(&self) -> Result<JournalSummary> {
        let conn = self.conn.lock();
        let count =
            |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get::<_, i64>(0))?) };
        let last_session = conn
            .query_row(
                "SELECT id, label, started_at, ended_at, tool_version, restore_point_seq
                 FROM sessions ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok(SessionInfo {
                        id: r.get(0)?,
                        label: r.get(1)?,
                        started_at: r.get(2)?,
                        ended_at: r.get(3)?,
                        tool_version: r.get(4)?,
                        restore_point_seq: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(JournalSummary {
            path: self.path.display().to_string(),
            sessions: count("SELECT COUNT(*) FROM sessions")?,
            registry_active: count("SELECT COUNT(*) FROM registry_journal WHERE active = 1")?,
            registry_total: count("SELECT COUNT(*) FROM registry_journal")?,
            services_active: count("SELECT COUNT(*) FROM service_journal WHERE active = 1")?,
            services_total: count("SELECT COUNT(*) FROM service_journal")?,
            scheduled_tasks_active: count(
                "SELECT COUNT(*) FROM scheduled_task_journal WHERE active = 1",
            )?,
            scheduled_tasks_total: count("SELECT COUNT(*) FROM scheduled_task_journal")?,
            appx_active: count("SELECT COUNT(*) FROM appx_journal WHERE active = 1")?,
            appx_total: count("SELECT COUNT(*) FROM appx_journal")?,
            power_active: count("SELECT COUNT(*) FROM power_journal WHERE active = 1")?,
            power_total: count("SELECT COUNT(*) FROM power_journal")?,
            dns_active: count("SELECT COUNT(*) FROM dns_journal WHERE active = 1")?,
            dns_total: count("SELECT COUNT(*) FROM dns_journal")?,
            task_definitions_active: count(
                "SELECT COUNT(*) FROM task_definition_journal WHERE active = 1",
            )?,
            task_definitions_total: count("SELECT COUNT(*) FROM task_definition_journal")?,
            last_session,
        })
    }

    pub fn export(&self) -> Result<JournalExport> {
        let registry = self
            .all_registry()?
            .into_iter()
            .map(|r| RegistryExportEntry {
                id: r.id,
                session_id: r.session_id,
                recorded_at: r.recorded_at.clone(),
                target: r.target(),
                hive: r.hive,
                key_path: r.key_path.clone(),
                value_name: r.value_name.clone(),
                key_existed: r.key_existed,
                value_existed: r.value_existed,
                original: r.original_decoded(),
                active: r.active,
                reverted_at: r.reverted_at.clone(),
            })
            .collect();
        let scheduled_tasks = self
            .all_scheduled_tasks()?
            .into_iter()
            .map(|r| ScheduledTaskExportEntry {
                id: r.id,
                session_id: r.session_id,
                recorded_at: r.recorded_at.clone(),
                target: r.target(),
                path: r.path.clone(),
                was_enabled: r.was_enabled,
                active: r.active,
                reverted_at: r.reverted_at.clone(),
            })
            .collect();
        let dns = self
            .all_dns()?
            .into_iter()
            .map(|r| DnsExportEntry {
                id: r.id,
                session_id: r.session_id,
                recorded_at: r.recorded_at.clone(),
                target: r.target(),
                interface_guid: r.interface_guid.clone(),
                family: r.family,
                adapter_name: r.adapter_name.clone(),
                previous_servers: network::split_servers(&r.previous_servers),
                target_servers: network::split_servers(&r.target_servers),
                active: r.active,
                reverted_at: r.reverted_at.clone(),
            })
            .collect();
        let task_definitions = self
            .all_task_definitions()?
            .into_iter()
            .map(|r| TaskDefinitionExportEntry {
                id: r.id,
                session_id: r.session_id,
                recorded_at: r.recorded_at.clone(),
                target: r.target(),
                path: r.path.clone(),
                purpose: r.purpose.clone(),
                folder_created: r.folder_created,
                active: r.active,
                reverted_at: r.reverted_at.clone(),
            })
            .collect();
        Ok(JournalExport {
            exported_at: now(),
            summary: self.summary()?,
            sessions: self.sessions()?,
            registry,
            services: self.all_services()?,
            scheduled_tasks,
            appx: self.all_appx()?,
            power: self.all_power()?,
            dns,
            task_definitions,
            ops: self.ops(500)?,
        })
    }

    pub fn export_json(&self) -> Result<String> {
        serde_json::to_string_pretty(&self.export()?).map_err(Error::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `registry_journal` as schema 2 created it, before `created_root`.
    const REGISTRY_V2: &str = r#"
        CREATE TABLE sessions (
            id                INTEGER PRIMARY KEY,
            started_at        TEXT    NOT NULL,
            ended_at          TEXT,
            label             TEXT    NOT NULL,
            tool_version      TEXT    NOT NULL,
            restore_point_seq INTEGER
        );
        CREATE TABLE registry_journal (
            id            INTEGER PRIMARY KEY,
            session_id    INTEGER NOT NULL REFERENCES sessions(id),
            recorded_at   TEXT    NOT NULL,
            hive          TEXT    NOT NULL,
            key_path      TEXT    NOT NULL,
            value_name    TEXT    NOT NULL,
            key_existed   INTEGER NOT NULL,
            value_existed INTEGER NOT NULL,
            value_type    INTEGER,
            value_data    BLOB,
            active        INTEGER NOT NULL DEFAULT 1,
            reverted_at   TEXT
        );
        INSERT INTO sessions (id, started_at, label, tool_version)
            VALUES (1, '2026-01-01T00:00:00Z', 'old', '0.1.0');
        INSERT INTO registry_journal
            (session_id, recorded_at, hive, key_path, value_name, key_existed, value_existed)
            VALUES (1, '2026-01-01T00:00:00Z', 'HKCU', 'Software\Old\Leaf', 'V', 0, 0);
        PRAGMA user_version = 2;
    "#;

    fn named_record(
        key_path: &str,
        value_name: &str,
        key_existed: bool,
        root: Option<&str>,
    ) -> NewRegistryRecord {
        NewRegistryRecord {
            hive: Hive::CurrentUser,
            key_path: key_path.to_string(),
            value_name: value_name.to_string(),
            key_existed,
            value_existed: false,
            original: None,
            created_root: root.map(str::to_string),
        }
    }

    fn new_record(key_path: &str, key_existed: bool, root: Option<&str>) -> NewRegistryRecord {
        named_record(key_path, "V", key_existed, root)
    }

    /// A record for `key_path` written after everything in the journal, as a query subject.
    fn newer_than_all(hive: Hive, key_path: &str) -> RegistryRecord {
        RegistryRecord {
            id: i64::MAX,
            session_id: 0,
            recorded_at: now(),
            hive,
            key_path: key_path.to_string(),
            value_name: "V".to_string(),
            key_existed: true,
            value_existed: false,
            original: None,
            active: true,
            reverted_at: None,
            created_root: None,
        }
    }

    /// Newest record of `key_path` and `value_name`, active or not.
    fn latest(journal: &Journal, key_path: &str, value_name: &str) -> RegistryRecord {
        journal
            .all_registry()
            .unwrap()
            .into_iter()
            .find(|r| r.key_path == key_path && r.value_name == value_name)
            .unwrap()
    }

    fn claims(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(path, root)| (path.to_string(), root.to_string()))
            .collect()
    }

    #[test]
    fn old_journal_gains_created_root_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(REGISTRY_V2)
            .unwrap();

        let journal = Journal::open(&path).unwrap();
        let rows = journal.active_registry().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key_path, r"Software\Old\Leaf");
        assert_eq!(rows[0].created_root, None);
        assert_eq!(
            journal
                .created_keys_before(&newer_than_all(Hive::CurrentUser, r"Software\Old\Leaf"))
                .unwrap(),
            claims(&[(r"Software\Old\Leaf", r"Software\Old\Leaf")]),
            "a row without created_root reports its own key as the root"
        );
        assert!(
            journal
                .created_keys_before(&newer_than_all(Hive::CurrentUser, r"Software\Old"))
                .unwrap()
                .is_empty(),
            "the parent of a created leaf existed"
        );
        let version: i64 = journal
            .conn
            .lock()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(version, 5);
        for table in [
            "scheduled_task_journal",
            "dns_journal",
            "task_definition_journal",
            "maintenance_runs",
        ] {
            let exists = journal
                .conn
                .lock()
                .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")
                .unwrap()
                .exists(params![table])
                .unwrap();
            assert!(exists, "{table} was not created");
        }
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());
        assert!(journal.active_dns().unwrap().is_empty());
        drop(journal);

        // Opening again finds the column and changes nothing.
        let journal = Journal::open(&path).unwrap();
        assert_eq!(journal.active_registry().unwrap().len(), 1);
    }

    #[test]
    fn scheduled_task_baseline_is_kept_until_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        let path = r"\Microsoft\Windows\Autochk\Proxy";
        let record = |path: &str, was_enabled: bool| {
            journal
                .record_scheduled_task(
                    session,
                    &NewScheduledTaskRecord {
                        path: path.to_string(),
                        was_enabled,
                    },
                )
                .unwrap()
        };

        assert!(record(path, true), "the first record is the baseline");
        assert!(
            !record(&path.to_uppercase(), false),
            "a path differing only in case keeps the baseline"
        );
        let active = journal.active_scheduled_tasks().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].path, path);
        assert!(active[0].was_enabled);
        assert_eq!(active[0].target(), format!("scheduled task {path}"));

        let summary = journal.summary().unwrap();
        assert_eq!(summary.scheduled_tasks_active, 1);
        assert_eq!(summary.scheduled_tasks_total, 1);
        assert!(summary.has_pending_changes());

        journal
            .mark_reverted(JournalTable::ScheduledTask, active[0].id)
            .unwrap();
        assert!(journal.active_scheduled_tasks().unwrap().is_empty());
        assert!(!journal.summary().unwrap().has_pending_changes());
        assert!(
            record(path, false),
            "a reverted baseline makes room for a new one"
        );

        let summary = journal.summary().unwrap();
        assert_eq!(summary.scheduled_tasks_active, 1);
        assert_eq!(summary.scheduled_tasks_total, 2);
        let export: serde_json::Value =
            serde_json::from_str(&journal.export_json().unwrap()).unwrap();
        let rows = export["scheduled_tasks"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["target"], format!("scheduled task {path}"));
        assert_eq!(rows[0]["was_enabled"], false);
        assert_eq!(rows[0]["active"], true);
        assert_eq!(rows[1]["active"], false);
        assert_eq!(export["summary"]["scheduled_tasks_active"], 1);
    }

    #[test]
    fn dns_baseline_is_kept_per_family() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        let record = |guid: &str, family: IpFamily, previous: &str| {
            journal
                .record_dns(
                    session,
                    &NewDnsRecord {
                        interface_guid: guid.to_string(),
                        family,
                        adapter_name: "Wi-Fi".to_string(),
                        previous_servers: previous.to_string(),
                        target_servers: "1.1.1.1,1.0.0.1".to_string(),
                    },
                )
                .unwrap()
        };
        let bare = "AAAAAAAA-0000-0000-0000-000000000001";
        let braced = "{aaaaaaaa-0000-0000-0000-000000000001}";

        assert!(record(bare, IpFamily::Ipv4, ""));
        assert!(record(bare, IpFamily::Ipv6, "2001:db8::1"));
        assert!(
            !record(braced, IpFamily::Ipv4, "9.9.9.9"),
            "bare and braced spellings of one GUID share a baseline"
        );

        let active = journal.active_dns().unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|r| r.interface_guid == braced));
        let v4 = active.iter().find(|r| r.family == IpFamily::Ipv4).unwrap();
        assert_eq!(v4.previous_servers, "");
        assert_eq!(v4.previous_text(), "automatic");
        assert_eq!(v4.target(), "IPv4 DNS servers of Wi-Fi");
        let v6 = active.iter().find(|r| r.family == IpFamily::Ipv6).unwrap();
        assert_eq!(v6.previous_text(), "2001:db8::1");

        let summary = journal.summary().unwrap();
        assert_eq!(summary.dns_active, 2);
        assert_eq!(summary.dns_total, 2);
        assert!(summary.has_pending_changes());

        let export: serde_json::Value =
            serde_json::from_str(&journal.export_json().unwrap()).unwrap();
        let rows = export["dns"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let v4 = rows.iter().find(|r| r["family"] == "ipv4").unwrap();
        assert_eq!(v4["previous_servers"], serde_json::json!([]));
        assert_eq!(
            v4["target_servers"],
            serde_json::json!(["1.1.1.1", "1.0.0.1"])
        );
        assert_eq!(v4["interface_guid"], braced);
        assert_eq!(v4["target"], "IPv4 DNS servers of Wi-Fi");

        journal
            .mark_reverted(JournalTable::Dns, v4["id"].as_i64().unwrap())
            .unwrap();
        assert_eq!(journal.summary().unwrap().dns_active, 1);
        assert_eq!(journal.all_dns().unwrap().len(), 2);
    }

    #[test]
    fn dns_target_update_keeps_the_baseline_of_the_active_record() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        let guid = "{aaaaaaaa-0000-0000-0000-000000000001}";
        let record = |family: IpFamily, previous: &str, target: &str| {
            journal
                .record_dns(
                    session,
                    &NewDnsRecord {
                        interface_guid: guid.to_string(),
                        family,
                        adapter_name: "Wi-Fi".to_string(),
                        previous_servers: previous.to_string(),
                        target_servers: target.to_string(),
                    },
                )
                .unwrap()
        };
        assert!(record(IpFamily::Ipv4, "10.0.0.53", "1.1.1.1,1.0.0.1"));
        assert!(record(IpFamily::Ipv6, "", "2606:4700:4700::1111"));

        let bare_upper = "AAAAAAAA-0000-0000-0000-000000000001";
        assert!(journal
            .update_dns_target(bare_upper, IpFamily::Ipv4, "8.8.8.8,8.8.4.4")
            .unwrap());
        let target = |family: IpFamily| {
            let rec = journal
                .active_dns()
                .unwrap()
                .into_iter()
                .find(|r| r.family == family)
                .unwrap();
            (rec.previous_servers, rec.target_servers)
        };
        assert_eq!(
            target(IpFamily::Ipv4),
            ("10.0.0.53".to_string(), "8.8.8.8,8.8.4.4".to_string())
        );
        assert_eq!(
            target(IpFamily::Ipv6),
            (String::new(), "2606:4700:4700::1111".to_string()),
            "the other family is left alone"
        );
        assert!(!journal
            .update_dns_target(
                "{aaaaaaaa-0000-0000-0000-000000000002}",
                IpFamily::Ipv4,
                "9.9.9.9"
            )
            .unwrap());

        let v4 = journal
            .active_dns()
            .unwrap()
            .into_iter()
            .find(|r| r.family == IpFamily::Ipv4)
            .unwrap();
        journal.mark_reverted(JournalTable::Dns, v4.id).unwrap();
        assert!(
            !journal
                .update_dns_target(guid, IpFamily::Ipv4, "9.9.9.9")
                .unwrap(),
            "a reverted record is not updated"
        );
        let reverted = journal
            .all_dns()
            .unwrap()
            .into_iter()
            .find(|r| r.id == v4.id)
            .unwrap();
        assert_eq!(reverted.target_servers, "8.8.8.8,8.8.4.4");
    }

    /// A schema 4 journal: the tables it had (only the ones these tests read) and one row.
    const JOURNAL_V4: &str = r#"
        CREATE TABLE sessions (
            id                INTEGER PRIMARY KEY,
            started_at        TEXT    NOT NULL,
            ended_at          TEXT,
            label             TEXT    NOT NULL,
            tool_version      TEXT    NOT NULL,
            restore_point_seq INTEGER
        );
        INSERT INTO sessions (id, started_at, label, tool_version)
            VALUES (1, '2026-01-01T00:00:00Z', 'old', '0.1.0');
        PRAGMA user_version = 4;
    "#;

    fn user_version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn data_dir_is_the_parent_of_the_default_journal() {
        assert_eq!(default_path(), data_dir().join("journal.db"));
        if let Some(dir) = std::env::var_os("OPTIMIZER_DATA_DIR") {
            assert_eq!(data_dir(), PathBuf::from(dir));
        }
    }

    #[test]
    fn newer_journal_is_refused_and_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE future (id INTEGER PRIMARY KEY, data TEXT);
                 INSERT INTO future (data) VALUES ('kept');
                 PRAGMA user_version = 6;",
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        let err = Journal::open(&path).unwrap_err();
        assert!(
            matches!(
                err,
                Error::JournalTooNew {
                    found: 6,
                    supported: 5
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "This change history was written by a newer version of Cairn (journal schema 6; this \
             version reads up to 5). Update Cairn to use it; nothing was changed."
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the file is unchanged"
        );
        assert!(!sidecar(&path, "-wal").exists(), "no WAL file appeared");
        assert!(!sidecar(&path, "-shm").exists());
        assert_eq!(user_version(&path), 6);
    }

    /// A WAL journal at `version` whose last writer closed without a checkpoint when
    /// `keep_frames`, so its committed pages are still only in the -wal file.
    fn wal_journal(path: &Path, version: i64, keep_frames: bool) {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE future (id INTEGER PRIMARY KEY, data TEXT);
             INSERT INTO future (data) VALUES ('kept');
             PRAGMA user_version = {version};"
        ))
        .unwrap();
        if keep_frames {
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
                .unwrap();
        }
    }

    #[test]
    fn newer_wal_journal_with_frames_is_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        wal_journal(&path, 6, true);
        let wal = sidecar(&path, "-wal");
        let before = (std::fs::read(&path).unwrap(), std::fs::read(&wal).unwrap());
        assert!(!before.1.is_empty(), "the WAL holds the committed pages");

        let err = Journal::open(&path).unwrap_err();
        assert!(
            matches!(
                err,
                Error::JournalTooNew {
                    found: 6,
                    supported: 5
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before.0,
            "the main file is unchanged"
        );
        assert_eq!(
            std::fs::read(&wal).ok(),
            Some(before.1),
            "the WAL is unchanged"
        );
        assert_eq!(user_version(&path), 6);
    }

    #[test]
    fn newer_wal_journal_closed_cleanly_is_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        wal_journal(&path, 6, false);
        assert!(!sidecar(&path, "-wal").exists());
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            Journal::open(&path),
            Err(Error::JournalTooNew { found: 6, .. })
        ));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "the file is unchanged"
        );
        assert!(!sidecar(&path, "-wal").exists(), "no WAL file appeared");
        assert!(!sidecar(&path, "-shm").exists());
    }

    #[test]
    fn an_older_wal_journal_is_checkpointed_when_it_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        wal_journal(&path, 4, true);
        assert!(sidecar(&path, "-wal").exists());

        let journal = Journal::open(&path).unwrap();
        assert_eq!(journal.summary().unwrap().pending_count(), 0);
        drop(journal);
        assert!(
            !sidecar(&path, "-wal").exists(),
            "the last connection to close checkpoints and removes the WAL as before"
        );
        assert_eq!(user_version(&path), SCHEMA_VERSION);
    }

    #[test]
    fn open_never_lowers_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch(JOURNAL_V4)
            .unwrap();

        let journal = Journal::open(&path).unwrap();
        assert_eq!(journal.sessions().unwrap().len(), 1, "rows are kept");
        assert!(journal.active_task_definitions().unwrap().is_empty());
        assert!(journal.maintenance_runs(10).unwrap().is_empty());
        drop(journal);
        assert_eq!(user_version(&path), 5, "a schema 4 journal opens as 5");

        let journal = Journal::open(&path).unwrap();
        drop(journal);
        assert_eq!(user_version(&path), 5, "opening it again leaves 5");

        // A newer version stored after the first read is kept by the stamp.
        let mut conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 7i64).unwrap();
        stamp_schema_version(&mut conn).unwrap();
        drop(conn);
        assert_eq!(user_version(&path), 7);
    }

    fn new_definition(path: &str, folder_created: bool) -> NewTaskDefinitionRecord {
        NewTaskDefinitionRecord {
            path: path.to_string(),
            purpose: "maintenance".to_string(),
            folder_created,
        }
    }

    #[test]
    fn task_definition_baseline_is_kept_until_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        let path = r"\Cairn\Maintenance-S-1-5-21-1111111111-2222222222-3333333333-1001";

        assert!(journal
            .record_task_definition(session, &new_definition(path, true))
            .unwrap());
        assert!(
            !journal
                .record_task_definition(session, &new_definition(&path.to_lowercase(), false))
                .unwrap(),
            "a path differing only in case keeps the baseline"
        );
        let active = journal.active_task_definitions().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].path, path);
        assert!(
            active[0].folder_created,
            "the first record's folder flag is kept"
        );
        assert_eq!(active[0].purpose, "maintenance");
        assert_eq!(active[0].target(), format!("task {path}"));
        assert_eq!(task_definition_target(path), format!("task {path}"));

        let summary = journal.summary().unwrap();
        assert_eq!(summary.task_definitions_active, 1);
        assert_eq!(summary.task_definitions_total, 1);
        assert_eq!(summary.pending_count(), 1);
        assert!(summary.has_pending_changes());

        journal
            .mark_reverted(JournalTable::TaskDefinition, active[0].id)
            .unwrap();
        assert!(journal.active_task_definitions().unwrap().is_empty());
        assert!(!journal.summary().unwrap().has_pending_changes());
        assert!(
            journal
                .record_task_definition(session, &new_definition(path, false))
                .unwrap(),
            "a reverted baseline makes room for a new one"
        );

        let all = journal.all_task_definitions().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].active && !all[0].folder_created, "newest first");
        assert!(!all[1].active && all[1].reverted_at.is_some());
        let export: serde_json::Value =
            serde_json::from_str(&journal.export_json().unwrap()).unwrap();
        let rows = export["task_definitions"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["target"], format!("task {path}"));
        assert_eq!(rows[0]["path"], path);
        assert_eq!(rows[0]["purpose"], "maintenance");
        assert_eq!(rows[0]["folder_created"], false);
        assert_eq!(rows[0]["active"], true);
        assert_eq!(rows[1]["folder_created"], true);
        assert_eq!(rows[1]["active"], false);
        assert_eq!(export["summary"]["task_definitions_active"], 1);
        assert_eq!(export["summary"]["task_definitions_total"], 2);
    }

    #[test]
    fn summary_without_task_definition_counters_still_deserializes() {
        let v4 = r#"{"path": "j.db", "sessions": 1, "registry_active": 0, "registry_total": 0,
            "services_active": 0, "services_total": 0, "scheduled_tasks_active": 0,
            "scheduled_tasks_total": 0, "appx_active": 0, "appx_total": 0, "power_active": 0,
            "power_total": 0, "dns_active": 1, "dns_total": 1, "last_session": null}"#;
        let summary: JournalSummary = serde_json::from_str(v4).unwrap();
        assert_eq!(summary.task_definitions_active, 0);
        assert_eq!(summary.task_definitions_total, 0);
        assert_eq!(summary.pending_count(), 1);
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["task_definitions_active"], 0);
        assert_eq!(json["task_definitions_total"], 0);
    }

    #[test]
    fn pending_count_sums_every_active_counter() {
        let mut summary: JournalSummary = serde_json::from_str(
            r#"{"path": "j.db", "sessions": 0, "registry_active": 0, "registry_total": 0,
                "services_active": 0, "services_total": 0, "appx_active": 0, "appx_total": 0,
                "power_active": 0, "power_total": 0, "last_session": null}"#,
        )
        .unwrap();
        assert_eq!(summary.pending_count(), 0);
        assert!(!summary.has_pending_changes());
        summary.registry_active = 1;
        summary.services_active = 2;
        summary.scheduled_tasks_active = 4;
        summary.appx_active = 8;
        summary.power_active = 16;
        summary.dns_active = 32;
        summary.task_definitions_active = 64;
        summary.registry_total = 1000;
        assert_eq!(summary.pending_count(), 127, "totals are not counted");
        assert!(summary.has_pending_changes());
        summary.registry_active = 0;
        summary.services_active = 0;
        summary.scheduled_tasks_active = 0;
        summary.appx_active = 0;
        summary.power_active = 0;
        summary.dns_active = 0;
        assert_eq!(summary.pending_count(), 64);
        assert!(
            summary.has_pending_changes(),
            "a task definition alone is pending"
        );
    }

    fn new_run(state: &str) -> NewMaintenanceRun {
        NewMaintenanceRun {
            origin: "task".to_string(),
            state: state.to_string(),
            request_json: r#"{"targets":["user_temp"]}"#.to_string(),
        }
    }

    #[test]
    fn maintenance_runs_insert_progress_finish_acknowledge() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let id = journal.insert_maintenance_run(&new_run("running")).unwrap();
        let runs = journal.maintenance_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, id);
        assert_eq!(runs[0].state, "running");
        assert_eq!(runs[0].origin, "task");
        assert_eq!(runs[0].request_json, r#"{"targets":["user_temp"]}"#);
        assert!(runs[0].ended_at.is_none() && runs[0].progress_json.is_none());
        assert!(
            !journal.acknowledge_maintenance_run(id).unwrap(),
            "a running run cannot be acknowledged"
        );

        journal
            .update_maintenance_progress(id, r#"{"step":"cleanup"}"#)
            .unwrap();
        journal
            .update_maintenance_progress(id, r#"{"step":"system_files"}"#)
            .unwrap();
        assert_eq!(
            journal.maintenance_runs(1).unwrap()[0]
                .progress_json
                .as_deref(),
            Some(r#"{"step":"system_files"}"#)
        );

        journal
            .finish_maintenance_run(id, "completed", r#"{"headline":"ok"}"#, Some(r"C:\log.log"))
            .unwrap();
        let run = &journal.maintenance_runs(1).unwrap()[0];
        assert_eq!(run.state, "completed");
        assert!(run.ended_at.is_some());
        assert_eq!(run.report_json.as_deref(), Some(r#"{"headline":"ok"}"#));
        assert_eq!(run.log_path.as_deref(), Some(r"C:\log.log"));
        assert!(run.acknowledged_at.is_none());

        assert!(journal.acknowledge_maintenance_run(id).unwrap());
        assert!(journal.maintenance_runs(1).unwrap()[0]
            .acknowledged_at
            .is_some());
        assert!(
            !journal.acknowledge_maintenance_run(id).unwrap(),
            "only the first acknowledgement counts"
        );
        assert!(!journal.acknowledge_maintenance_run(id + 100).unwrap());

        // Finishing again without a log path keeps the stored one.
        journal
            .finish_maintenance_run(id, "failed", "{}", None)
            .unwrap();
        assert_eq!(
            journal.maintenance_runs(1).unwrap()[0].log_path.as_deref(),
            Some(r"C:\log.log")
        );
        let summary = journal.summary().unwrap();
        assert!(
            !summary.has_pending_changes(),
            "runs are never pending changes"
        );
        assert_eq!(summary.sessions, 0, "runs open no session");
    }

    #[test]
    fn interrupt_marks_only_running_rows() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let first = journal.insert_maintenance_run(&new_run("running")).unwrap();
        let done = journal.insert_maintenance_run(&new_run("running")).unwrap();
        journal
            .finish_maintenance_run(done, "completed", "{}", None)
            .unwrap();
        let second = journal.insert_maintenance_run(&new_run("running")).unwrap();

        assert_eq!(
            journal.interrupt_running_maintenance_runs().unwrap(),
            vec![first, second]
        );
        let runs = journal.maintenance_runs(10).unwrap();
        let state = |id: i64| runs.iter().find(|r| r.id == id).unwrap().clone();
        assert_eq!(state(first).state, "interrupted");
        assert!(state(first).ended_at.is_some());
        assert_eq!(state(second).state, "interrupted");
        assert_eq!(state(done).state, "completed");
        assert!(journal
            .interrupt_running_maintenance_runs()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn interrupting_one_run_leaves_every_other_row() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let stale = journal.insert_maintenance_run(&new_run("running")).unwrap();
        let done = journal.insert_maintenance_run(&new_run("running")).unwrap();
        journal
            .finish_maintenance_run(done, "completed", "{}", None)
            .unwrap();
        let newer = journal.insert_maintenance_run(&new_run("running")).unwrap();

        assert!(journal.interrupt_maintenance_run(stale).unwrap());
        let runs = journal.maintenance_runs(10).unwrap();
        let state = |id: i64| runs.iter().find(|r| r.id == id).unwrap().clone();
        assert_eq!(state(stale).state, "interrupted");
        assert!(state(stale).ended_at.is_some());
        assert_eq!(state(newer).state, "running", "another running row stays");
        assert!(state(newer).ended_at.is_none());
        assert_eq!(state(done).state, "completed");

        assert!(
            !journal.interrupt_maintenance_run(stale).unwrap(),
            "a run is interrupted once"
        );
        assert!(
            !journal.interrupt_maintenance_run(done).unwrap(),
            "a finished run keeps its state"
        );
        assert!(!journal.interrupt_maintenance_run(newer + 100).unwrap());
        let runs = journal.maintenance_runs(10).unwrap();
        let states: Vec<(i64, &str)> = runs.iter().map(|r| (r.id, r.state.as_str())).collect();
        assert_eq!(
            states,
            vec![
                (newer, "running"),
                (done, "completed"),
                (stale, "interrupted")
            ]
        );
    }

    #[test]
    fn prune_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let running = journal.insert_maintenance_run(&new_run("running")).unwrap();
        let mut finished = Vec::new();
        for _ in 0..5 {
            let id = journal.insert_maintenance_run(&new_run("running")).unwrap();
            journal
                .finish_maintenance_run(id, "completed", "{}", None)
                .unwrap();
            finished.push(id);
        }

        assert_eq!(journal.prune_maintenance_runs(2).unwrap(), 3);
        let ids: Vec<i64> = journal
            .maintenance_runs(10)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            ids,
            vec![finished[4], finished[3], running],
            "the newest two and the run still in progress stay, newest first"
        );
        assert_eq!(journal.prune_maintenance_runs(2).unwrap(), 0);
        assert_eq!(journal.maintenance_runs(1).unwrap()[0].id, finished[4]);
    }

    #[test]
    fn summary_without_the_new_counters_still_deserializes() {
        let old = r#"{"path": "j.db", "sessions": 1, "registry_active": 1, "registry_total": 1,
            "services_active": 0, "services_total": 0, "appx_active": 0, "appx_total": 0,
            "power_active": 0, "power_total": 0, "last_session": null}"#;
        let summary: JournalSummary = serde_json::from_str(old).unwrap();
        assert_eq!(summary.scheduled_tasks_active, 0);
        assert_eq!(summary.dns_total, 0);
        assert!(summary.has_pending_changes());
    }

    #[test]
    fn created_root_round_trips_and_is_dropped_for_existing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        journal
            .record_registry(session, &new_record(r"A\B\C", false, Some(r"A\B")))
            .unwrap();
        journal
            .record_registry(session, &new_record(r"X\Y", true, Some("X")))
            .unwrap();

        let rows = journal.active_registry().unwrap();
        let by_path = |p: &str| rows.iter().find(|r| r.key_path == p).unwrap();
        assert_eq!(by_path(r"A\B\C").created_root.as_deref(), Some(r"A\B"));
        assert_eq!(by_path(r"X\Y").created_root, None);

        let before = |hive: Hive, key_path: &str| {
            journal
                .created_keys_before(&newer_than_all(hive, key_path))
                .unwrap()
        };
        let expected = claims(&[(r"A\B\C", r"A\B")]);
        assert_eq!(before(Hive::CurrentUser, r"A\B\C"), expected);
        assert_eq!(before(Hive::CurrentUser, r"a\b\other"), expected);
        assert!(before(Hive::CurrentUser, r"A\Other").is_empty());
        assert!(before(Hive::CurrentUser, r"X\Y").is_empty());
        assert!(before(Hive::LocalMachine, r"A\B\C").is_empty());
    }

    #[test]
    fn records_reverted_before_a_record_was_written_claim_no_keys_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        let find = |path: &str| latest(&journal, path, "V");

        // Created A and was reverted before B was written: B's baseline had A.
        journal
            .record_registry(session, &new_record(r"A\Old", false, Some("A")))
            .unwrap();
        journal
            .mark_reverted(JournalTable::Registry, find(r"A\Old").id)
            .unwrap();
        // Created S and is reverted only after C was written: C still relies on it.
        journal
            .record_registry(session, &new_record(r"S\X", false, Some("S")))
            .unwrap();
        journal
            .record_registry(session, &new_record(r"A\B", false, Some(r"A\B")))
            .unwrap();
        journal
            .record_registry(session, &new_record(r"S\Y", false, Some(r"S\Y")))
            .unwrap();
        journal
            .mark_reverted(JournalTable::Registry, find(r"S\X").id)
            .unwrap();

        assert!(journal
            .created_keys_before(&find(r"A\B"))
            .unwrap()
            .is_empty());
        assert_eq!(
            journal.created_keys_before(&find(r"S\Y")).unwrap(),
            claims(&[(r"S\X", "S")])
        );
    }

    #[test]
    fn created_keys_kept_by_other_values_stay_claimed_after_their_creator_is_reverted() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        let (root, key) = ("Approved", r"Approved\Run");
        let record = |name: &str, key_existed: bool| {
            let root = (!key_existed).then_some(root);
            assert!(journal
                .record_registry(session, &named_record(key, name, key_existed, root))
                .unwrap());
        };
        let revert = |name: &str| {
            journal
                .mark_reverted(JournalTable::Registry, latest(&journal, key, name).id)
                .unwrap();
        };

        // First creates both keys; Second then finds them.
        record("First", false);
        record("Second", true);
        // First is reverted while Second keeps the keys, then written again.
        revert("First");
        record("First", true);
        // Second is reverted while the new First keeps the keys.
        revert("Second");
        assert_eq!(
            journal
                .created_keys_before(&latest(&journal, key, "First"))
                .unwrap(),
            claims(&[(key, root)]),
            "the keys stood from the first write on, so the last revert may remove them"
        );

        // Once no journaled value is left below the keys they may have been removed, so a
        // later record that finds them does not inherit the claim.
        revert("First");
        record("Third", true);
        assert!(journal
            .created_keys_before(&latest(&journal, key, "Third"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_claim_that_outlives_its_creator_covers_only_the_keys_still_held() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Journal::open(dir.path().join("journal.db")).unwrap();
        let session = journal.begin_session("test", "0").unwrap();
        // Creator makes R, R\X and R\X\Leaf; a sibling value under R keeps only R alive.
        journal
            .record_registry(session, &new_record(r"R\X\Leaf", false, Some("R")))
            .unwrap();
        journal
            .record_registry(session, &new_record(r"R\Y", false, Some(r"R\Y")))
            .unwrap();
        journal
            .mark_reverted(
                JournalTable::Registry,
                latest(&journal, r"R\X\Leaf", "V").id,
            )
            .unwrap();
        // Something else recreated R\X\Leaf before this write found it.
        journal
            .record_registry(session, &new_record(r"R\X\Leaf", true, None))
            .unwrap();

        assert_eq!(
            journal
                .created_keys_before(&latest(&journal, r"R\X\Leaf", "V"))
                .unwrap(),
            claims(&[("R", "R")])
        );
    }
}
