//! ToolsDB (MariaDB) backend.
//!
//! The database is the durable store, shared by the bot and the web service.
//! Recent rows are mirrored in the in-memory [`Cache`]; the web UI reads that
//! mirror, so its handlers stay synchronous and cheap.

use anyhow::{Context, Result, anyhow, bail};

use std::collections::VecDeque;
use std::time::Duration;

use toolforge::pool::ToolsDbPool;
use toolforge::pool::mysql_async::{Conn, Params, Row, Value, prelude::*};

use super::Cache;
use crate::config::{Config, StoreConfig};
use crate::mediawiki::Cursor;
use crate::model::{Detection, Level};

/// The database name when `store.database` is empty.
const DATABASE_SUFFIX: &str = "__faike_p";

/// Schema, created idempotently by both processes so a fresh deployment is
/// self-healing.
const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS detections (
        id VARCHAR(255) NOT NULL,
        title VARCHAR(255) NOT NULL,
        timestamp VARCHAR(32) NOT NULL,
        `user` VARCHAR(255) NOT NULL,
        comment MEDIUMTEXT NOT NULL,
        url MEDIUMTEXT NOT NULL,
        description_url MEDIUMTEXT NOT NULL,
        mime VARCHAR(64) NOT NULL,
        size BIGINT UNSIGNED NOT NULL,
        sha1 VARCHAR(40) NOT NULL,
        score TINYINT UNSIGNED NOT NULL,
        signals MEDIUMTEXT NOT NULL,
        created TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        PRIMARY KEY (id),
        KEY idx_timestamp (timestamp),
        KEY idx_score_timestamp (score, timestamp),
        KEY idx_sha1 (sha1)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
    "CREATE TABLE IF NOT EXISTS meta (
        `key` VARCHAR(64) NOT NULL,
        `value` MEDIUMTEXT NOT NULL,
        PRIMARY KEY (`key`)
    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
];

const INSERT: &str = "INSERT IGNORE INTO detections
    (id, title, timestamp, `user`, comment, url, description_url, mime, size, sha1, score, signals)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const SELECT_RECENT: &str = "SELECT id, title, timestamp, `user`, comment, url,
    description_url, mime, size, sha1, score, signals
    FROM detections ORDER BY timestamp DESC, id DESC LIMIT ?";

const SELECT_CURSOR: &str = "SELECT `value` FROM meta WHERE `key` = 'cursor'";

const UPSERT_CURSOR: &str = "INSERT INTO meta (`key`, `value`) VALUES ('cursor', ?)
    ON DUPLICATE KEY UPDATE `value` = ?";

pub struct ToolsDbStore {
    pool: ToolsDbPool,
    database: String,
    cache: Cache,
    read_only: bool,
}

impl ToolsDbStore {
    pub async fn open(config: &Config) -> Result<Self> {
        Self::build(config, false).await
    }

    pub async fn open_read_only(config: &Config) -> Result<Self> {
        Self::build(config, true).await
    }

    async fn build(config: &Config, read_only: bool) -> Result<Self> {
        let pool = build_pool(&config.store)?;
        let database = database_name(&config.store, &pool);
        ensure_schema(&pool, &database).await?;
        // Seed from the primary: the schema was just ensured there, and the
        // read-only replica may lag on a fresh deployment.
        let mut conn = connect(&pool, &database, false).await?;
        let records = load_recent(&mut conn, config.max_records).await?;
        drop(conn);
        let cache = Cache::new(config.max_records);
        cache.replace(records);
        Ok(Self {
            pool,
            database,
            cache,
            read_only,
        })
    }

    /// Reload the in-memory ring from the database.
    pub async fn reload(&self) -> Result<()> {
        let mut conn = self.connect().await?;
        let records = load_recent(&mut conn, self.cache.max_records()).await?;
        self.cache.replace(records);
        Ok(())
    }

    pub fn is_seen(&self, id: &str) -> bool {
        self.cache.is_seen(id)
    }

    pub fn mark_seen(&self, id: &str) {
        self.cache.mark_seen(id);
    }

    /// Store a detection. Returns `false` if the id was already stored, which
    /// `INSERT IGNORE` also enforces across processes.
    pub async fn add(&self, detection: Detection) -> Result<bool> {
        if self.read_only {
            bail!("Detection store is read-only");
        }
        if self.cache.is_seen(&detection.id) {
            return Ok(false);
        }
        let signals =
            serde_json::to_string(&detection.signals).context("Serializing detection signals")?;
        let params = Params::Positional(vec![
            Value::from(detection.id.clone()),
            Value::from(detection.title.clone()),
            Value::from(detection.timestamp.clone()),
            Value::from(detection.user.clone()),
            Value::from(detection.comment.clone()),
            Value::from(detection.url.clone()),
            Value::from(detection.description_url.clone()),
            Value::from(detection.mime.clone()),
            Value::from(detection.size),
            Value::from(detection.sha1.clone()),
            Value::from(detection.score),
            Value::from(signals),
        ]);
        let mut conn = self.connect().await?;
        conn.exec_drop(INSERT, params)
            .await
            .with_context(|| format!("Inserting into ToolsDB {}", self.database))?;
        if conn.affected_rows() == 0 {
            self.cache.mark_seen(&detection.id);
            return Ok(false);
        }
        self.cache.push(detection);
        Ok(true)
    }

    pub fn recent(&self, filter: &super::Filter) -> Vec<Detection> {
        self.cache.recent(filter)
    }

    pub fn count(&self) -> usize {
        self.cache.count()
    }

    pub async fn load_cursor(&self) -> Option<Cursor> {
        let mut conn = self.connect().await.ok()?;
        let value: Option<String> = conn.query_first(SELECT_CURSOR).await.ok()?;
        serde_json::from_str(&value?).ok()
    }

    pub async fn save_cursor(&self, cursor: &Cursor) -> Result<()> {
        if self.read_only {
            bail!("Detection store is read-only");
        }
        let value = serde_json::to_string(cursor).context("Serializing cursor")?;
        let mut conn = self.connect().await?;
        conn.exec_drop(UPSERT_CURSOR, (value.as_str(), value.as_str()))
            .await
            .with_context(|| format!("Saving the cursor to ToolsDB {}", self.database))?;
        Ok(())
    }

    async fn connect(&self) -> Result<Conn> {
        connect(&self.pool, &self.database, self.read_only).await
    }
}

fn build_pool(store: &StoreConfig) -> Result<ToolsDbPool> {
    let mut builder = ToolsDbPool::builder()
        .pool_max(store.pool_max)
        .connect_timeout(Duration::from_secs(store.connect_timeout_sec));
    if store.max_statement_time_sec > 0 {
        builder = builder.max_statement_time(Duration::from_secs(store.max_statement_time_sec));
    }
    if let Some(path) = &store.replica_my_cnf {
        builder = builder.my_cnf_path(path);
    }
    builder
        .build()
        .map_err(|error| match &store.replica_my_cnf {
            Some(path) => anyhow!(
                "Cannot load ToolsDB credentials from {}: {error}",
                path.display()
            ),
            None => anyhow!("Cannot load ToolsDB credentials: {error}"),
        })
}

fn database_name(store: &StoreConfig, pool: &ToolsDbPool) -> String {
    if store.database.is_empty() {
        format!("{}{DATABASE_SUFFIX}", pool.user())
    } else {
        store.database.clone()
    }
}

async fn connect(pool: &ToolsDbPool, database: &str, read_only: bool) -> Result<Conn> {
    let result = if read_only {
        pool.connect_readonly(database).await
    } else {
        pool.connect(database).await
    };
    result.with_context(|| format!("Cannot connect to ToolsDB {database}"))
}

async fn ensure_schema(pool: &ToolsDbPool, database: &str) -> Result<()> {
    pool.create_database(database)
        .await
        .with_context(|| format!("Cannot create ToolsDB database {database}"))?;
    let mut conn = connect(pool, database, false).await?;
    for statement in SCHEMA {
        conn.query_drop(*statement)
            .await
            .with_context(|| format!("Cannot create a table in ToolsDB {database}"))?;
    }
    Ok(())
}

async fn load_recent(conn: &mut Conn, limit: usize) -> Result<VecDeque<Detection>> {
    let rows: Vec<Row> = conn
        .exec(SELECT_RECENT, (limit,))
        .await
        .context("Reading recent detections from ToolsDB")?;
    Ok(rows.into_iter().filter_map(detection_from_row).collect())
}

/// Rows written by an older schema or a partial insert are skipped rather
/// than failing the whole reload.
fn detection_from_row(row: Row) -> Option<Detection> {
    let score: u8 = row.get("score")?;
    let signals = serde_json::from_str(&row.get::<String, _>("signals")?).ok()?;
    Some(Detection {
        id: row.get("id")?,
        title: row.get("title")?,
        timestamp: row.get("timestamp")?,
        user: row.get("user")?,
        comment: row.get("comment")?,
        url: row.get("url")?,
        description_url: row.get("description_url")?,
        mime: row.get("mime")?,
        size: row.get("size")?,
        sha1: row.get("sha1")?,
        score,
        level: Level::from_score(score),
        signals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::detection;

    fn config(database: &str, my_cnf: std::path::PathBuf) -> Config {
        Config {
            store: StoreConfig {
                kind: crate::config::StoreKind::Toolsdb,
                database: database.to_string(),
                replica_my_cnf: Some(my_cnf),
                ..StoreConfig::default()
            },
            max_records: 10,
            ..Config::default()
        }
    }

    /// A credentials file for `build_pool`, so this test needs no real one.
    fn fake_my_cnf() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("replica.my.cnf");
        std::fs::write(
            &path,
            "[client]\nuser = s12345\npassword = secret\nlocal = 'true'\n\n[tunnels]\ntoolsdb = 3306\n",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn database_name_derives_from_the_tool_user() {
        let (_dir, my_cnf) = fake_my_cnf();
        let config = config("", my_cnf);
        let pool = build_pool(&config.store).unwrap();
        assert_eq!(
            database_name(&config.store, &pool),
            format!("{}__faike_p", pool.user())
        );
    }

    /// Round-trip against the real ToolsDB. Opt in with:
    /// `AIMER_TOOLSDB_TEST=1 cargo test -- --ignored`
    #[tokio::test]
    #[ignore = "needs the ToolsDB tunnel and credentials"]
    async fn round_trips_against_toolsdb() {
        if std::env::var_os("AIMER_TOOLSDB_TEST").is_none() {
            return;
        }
        let my_cnf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("replica.my.cnf");
        if !my_cnf.exists() {
            return;
        }
        let config = config("s58339__faike_p", my_cnf);
        let store = ToolsDbStore::open(&config).await.unwrap();
        let id = format!("aimer-test-{}", std::process::id());
        let detection = detection(&id, 90);
        assert!(store.add(detection.clone()).await.unwrap());
        assert!(!store.add(detection.clone()).await.unwrap(), "deduplicated");
        store.reload().await.unwrap();
        assert!(store.is_seen(&id));

        let mut conn = store.connect().await.unwrap();
        conn.exec_drop("DELETE FROM detections WHERE id = ?", (id,))
            .await
            .unwrap();
    }
}
