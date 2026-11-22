//! The open sequence, connection setup, header checks, migrations, and schema fingerprints
//! (`docs/design/event-store.md` §2.8).

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags};
use tokio::sync::broadcast;

use super::content::sha256_hex;
use super::error::{OpenError, StoreError};
use super::readers::Pool;
use super::schema::{APPLICATION_ID, FINGERPRINTS, MIGRATIONS};
use super::writer::{self, Faults, WriterCtx};
use super::{Inner, Shared, Store, StoreConfig, StorePaths};

/// Which connection is being set up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Writer,
    Reader,
}

/// The VFS every store connection uses: on Windows `win32-longpath`, since the default
/// `win32` VFS limits a path to 1040 UTF-8 bytes.
#[cfg(windows)]
const VFS: Option<&str> = Some("win32-longpath");
#[cfg(not(windows))]
const VFS: Option<&str> = None;

/// Opens a connection to an existing file with the store's settings (no `SQLITE_OPEN_CREATE`).
pub(crate) fn connect(
    path: &Path,
    role: Role,
    busy_timeout: Duration,
) -> Result<Connection, OpenError> {
    let flags = match role {
        Role::Writer => OpenFlags::SQLITE_OPEN_READ_WRITE,
        Role::Reader => OpenFlags::SQLITE_OPEN_READ_ONLY,
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = match VFS {
        Some(vfs) => Connection::open_with_flags_and_vfs(path, flags, vfs),
        None => Connection::open_with_flags(path, flags),
    }
    .map_err(|e| {
        OpenError::Io(format!(
            "open {} ({} UTF-8 bytes) for {role:?}: {}",
            path.display(),
            path.as_os_str().len(),
            StoreError::from(e)
        ))
    })?;
    configure(&conn, role, busy_timeout)?;
    Ok(conn)
}

/// The per-connection settings, none of which writes the file, each read back.
pub(crate) fn configure(
    conn: &Connection,
    role: Role,
    busy_timeout: Duration,
) -> Result<(), OpenError> {
    conn.busy_timeout(busy_timeout)?;
    let ms = i64::try_from(busy_timeout.as_millis()).unwrap_or(i64::MAX);
    expect_pragma(conn, "busy_timeout", ms)?;
    for (name, value) in [
        ("foreign_keys", 1),
        ("recursive_triggers", 1),
        ("trusted_schema", 0),
        ("synchronous", 2),
        ("fullfsync", 1),
        ("checkpoint_fullfsync", 1),
        ("journal_size_limit", 67_108_864),
    ] {
        conn.pragma_update(None, name, value)?;
        expect_pragma(conn, name, value)?;
    }
    if !conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)? {
        return Err(OpenError::Setup(
            "SQLITE_DBCONFIG_DEFENSIVE did not take effect".into(),
        ));
    }
    if role == Role::Reader {
        conn.pragma_update(None, "query_only", 1)?;
        expect_pragma(conn, "query_only", 1)?;
    }
    Ok(())
}

fn expect_pragma(conn: &Connection, name: &str, want: i64) -> Result<(), OpenError> {
    let got: i64 = conn.pragma_query_value(None, name, |r| r.get(0))?;
    if got == want {
        Ok(())
    } else {
        Err(OpenError::Setup(format!(
            "PRAGMA {name} is {got}, expected {want}"
        )))
    }
}

/// What step 3 found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    Fresh,
    Ours { user_version: u32 },
}

/// Step 3: read-only checks of an existing file, in one read transaction.
fn classify(conn: &Connection) -> Result<Found, OpenError> {
    conn.execute_batch("BEGIN DEFERRED")?;
    let result = (|| {
        // Fixed at creation; an empty file reports the encoding it will be created with.
        let encoding: String = conn.pragma_query_value(None, "encoding", |r| r.get(0))?;
        if encoding != "UTF-8" {
            return Err(OpenError::Encoding(encoding));
        }
        let app: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let objects: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
        if app == 0 && objects == 0 {
            return Ok(Found::Fresh);
        }
        if app != APPLICATION_ID {
            return Err(OpenError::Foreign {
                application_id: app,
            });
        }
        let user_version = user_version(conn)?;
        let supported = supported_version();
        if user_version > supported {
            return Err(OpenError::NewerSchema {
                found: user_version,
                supported,
            });
        }
        check_fingerprint(conn, user_version)?;
        Ok(Found::Ours { user_version })
    })();
    let _ = conn.execute_batch("COMMIT");
    result
}

fn user_version(conn: &Connection) -> Result<u32, OpenError> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    u32::try_from(v).map_err(|_| OpenError::Setup(format!("user_version {v} is negative")))
}

pub(crate) fn supported_version() -> u32 {
    u32::try_from(MIGRATIONS.len()).unwrap_or(u32::MAX)
}

/// The schema dump the fingerprint hashes: `type`, `name`, `tbl_name`, `sql` of every
/// `sqlite_schema` row except SQLite's internal objects, sorted. Internal objects are
/// selected by the literal prefix `sqlite_` (not `LIKE 'sqlite_%'`, whose `_` matches any
/// character and would hide a table such as `sqliteXextra`).
pub(crate) fn schema_dump(conn: &Connection) -> Result<Vec<[String; 4]>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, coalesce(sql, '') FROM sqlite_schema
         WHERE lower(substr(name, 1, 7)) <> 'sqlite_'
         ORDER BY type, name",
    )?;
    let rows = stmt.query_map([], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?]))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn fingerprint_of(dump: &[[String; 4]]) -> String {
    let mut text = String::new();
    for row in dump {
        for (i, col) in row.iter().enumerate() {
            if i > 0 {
                text.push('\u{1f}');
            }
            text.push_str(col);
        }
        text.push('\u{1e}');
    }
    sha256_hex(text.as_bytes())
}

/// The schema dump of `version` as the shipped migrations create it.
pub(crate) fn expected_dump(version: u32) -> Result<Vec<[String; 4]>, StoreError> {
    let conn = Connection::open_in_memory()?;
    for script in MIGRATIONS.iter().take(version as usize) {
        conn.execute_batch(script)?;
    }
    schema_dump(&conn)
}

fn check_fingerprint(conn: &Connection, version: u32) -> Result<(), OpenError> {
    let dump = schema_dump(conn)?;
    let shipped = version
        .checked_sub(1)
        .and_then(|i| FINGERPRINTS.get(i as usize))
        .copied()
        .unwrap_or_default();
    if fingerprint_of(&dump) == shipped {
        return Ok(());
    }
    // Name the objects that differ from what the shipped migrations create.
    let expected = expected_dump(version)?;
    let mut names = Vec::new();
    for row in &dump {
        if !expected.contains(row) {
            names.push(row[1].clone());
        }
    }
    for row in &expected {
        if !dump.iter().any(|d| d[1] == row[1]) {
            names.push(row[1].clone());
        }
    }
    if names.is_empty() {
        names.push(format!("(fingerprint of version {version})"));
    }
    names.sort();
    names.dedup();
    Err(OpenError::SchemaMismatch { version, names })
}

/// Step 1: create the file if missing (mode 0600 on Unix); another opener's file is used
/// as it is. `open` never creates directories and never deletes a file.
fn create_if_missing(path: &Path) -> Result<(), OpenError> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(OpenError::Io(format!("create {}: {e}", path.display()))),
    }
}

/// On Windows the canonical `\\?\` path of the state directory, joined with the file name;
/// elsewhere the path as given.
fn db_path(path: &Path) -> Result<PathBuf, OpenError> {
    if cfg!(windows) {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = path
            .file_name()
            .ok_or_else(|| OpenError::Io(format!("{} has no file name", path.display())))?;
        let dir = std::fs::canonicalize(dir).map_err(|e| {
            OpenError::Io(format!(
                "canonicalize {} ({} bytes): {e}",
                dir.display(),
                dir.as_os_str().len()
            ))
        })?;
        Ok(dir.join(name))
    } else {
        Ok(path.to_path_buf())
    }
}

/// Step 5 for a fresh file, under the write lock: re-read the header, then run every
/// migration and write `meta`.
fn initialize_or_migrate(
    conn: &Connection,
    cfg: &StoreConfig,
    projection_version: u32,
) -> Result<(), OpenError> {
    let supported = supported_version();
    loop {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let step = (|| -> Result<bool, OpenError> {
            let app: i32 = conn.pragma_query_value(None, "application_id", |r| r.get(0))?;
            let objects: i64 =
                conn.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            let version = if app == 0 && objects == 0 {
                0
            } else if app != APPLICATION_ID {
                return Err(OpenError::Foreign {
                    application_id: app,
                });
            } else {
                user_version(conn)?
            };
            if version > supported {
                return Err(OpenError::NewerSchema {
                    found: version,
                    supported,
                });
            }
            if version == supported {
                return Ok(true);
            }
            if version > 0 {
                check_fingerprint(conn, version)?;
            }
            conn.execute_batch(MIGRATIONS[version as usize])?;
            if version == 0 {
                conn.pragma_update(None, "application_id", APPLICATION_ID)?;
                let (_, now) = cfg.clock.now();
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('epoch', ?1), ('created_at', ?2), ('projection_version', ?3)",
                    rusqlite::params![cfg.ids.epoch().0, now.0, projection_version.to_string()],
                )?;
            }
            conn.pragma_update(None, "user_version", version + 1)?;
            Ok(false)
        })();
        match step {
            Ok(done) => {
                conn.execute_batch(if done { "ROLLBACK" } else { "COMMIT" })?;
                if done {
                    break;
                }
            }
            Err(e) => {
                if !conn.is_autocommit() {
                    let _ = conn.execute_batch("ROLLBACK");
                }
                return Err(e);
            }
        }
    }
    check_fingerprint(conn, supported)
}

/// Checks the configuration's ranges before anything touches the path.
fn check_config(cfg: &StoreConfig) -> Result<(), OpenError> {
    if !(1..=super::MAX_READERS).contains(&cfg.readers) {
        return Err(OpenError::Config(format!(
            "readers {} is outside 1..={}",
            cfg.readers,
            super::MAX_READERS
        )));
    }
    if !(1..=super::MAX_WRITER_QUEUE).contains(&cfg.writer_queue) {
        return Err(OpenError::Config(format!(
            "writer_queue {} is outside 1..={}",
            cfg.writer_queue,
            super::MAX_WRITER_QUEUE
        )));
    }
    if i32::try_from(cfg.busy_timeout.as_millis()).is_err() {
        return Err(OpenError::Config(format!(
            "busy_timeout {:?} exceeds i32::MAX milliseconds",
            cfg.busy_timeout
        )));
    }
    Ok(())
}

pub(crate) fn open(cfg: StoreConfig, faults: Arc<Faults>) -> Result<Store, OpenError> {
    check_config(&cfg)?;
    let projection_version = faults
        .projection_version
        .unwrap_or(super::schema::PROJECTION_VERSION);
    let path = db_path(&cfg.path)?;
    // 1. Create if missing.
    create_if_missing(&path)?;
    if let Some(hook) = &faults.after_create {
        hook();
    }
    // 2. Writer connection.
    let conn = connect(&path, Role::Writer, cfg.busy_timeout)?;
    // 3. Read-only checks, before any change.
    let found = classify(&conn)?;
    // 4. WAL.
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(OpenError::NoWal(mode));
    }
    // 5. Initialize or migrate under the lock.
    let needs_init = match found {
        Found::Fresh => true,
        Found::Ours { user_version } => user_version < supported_version(),
    };
    if needs_init {
        initialize_or_migrate(&conn, &cfg, projection_version)?;
    }
    if faults.fail_open_after_init.load(Ordering::SeqCst) {
        return Err(OpenError::Io(
            "injected failure after initialization".into(),
        ));
    }
    // 6. Projections.
    let stored: String = conn.query_row(
        "SELECT value FROM meta WHERE key = 'projection_version'",
        [],
        |r| r.get(0),
    )?;
    if stored != projection_version.to_string() {
        writer::rebuild_at_open(&conn, &cfg, projection_version)?;
    }
    let epoch = chaldea_core::ids::Epoch(conn.query_row(
        "SELECT value FROM meta WHERE key = 'epoch'",
        [],
        |r| r.get(0),
    )?);
    // 7. Readers and the writer thread.
    let mut readers = Vec::with_capacity(cfg.readers);
    for _ in 0..cfg.readers {
        readers.push(connect(&path, Role::Reader, cfg.busy_timeout)?);
    }
    let pool = Arc::new(Pool::new(readers, faults.clone()));
    let (tx, rx) = mpsc::sync_channel(cfg.writer_queue);
    let shared = Arc::new(Shared::new(tx));
    let (notifier, _) = broadcast::channel(1024);
    let join = writer::spawn(WriterCtx {
        conn: Some(conn),
        rx,
        clock: cfg.clock,
        ids: cfg.ids,
        shared: shared.clone(),
        notifier: notifier.clone(),
        faults: faults.clone(),
        projection_version,
        jobs_run: 0,
        jobs_expired: 0,
    })
    .map_err(|e| OpenError::Io(format!("spawn the writer thread: {e}")))?;
    let mut paths_wal = path.clone().into_os_string();
    paths_wal.push("-wal");
    let mut paths_shm = path.clone().into_os_string();
    paths_shm.push("-shm");
    Ok(Store {
        inner: Arc::new(Inner {
            shared,
            pool,
            epoch,
            paths: StorePaths {
                db: path,
                wal: paths_wal.into(),
                shm: paths_shm.into(),
            },
            notifier: Mutex::new(Some(notifier)),
            writer: Mutex::new(Some(join)),
            faults,
        }),
    })
}
