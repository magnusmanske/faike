//! ToolsDB (MariaDB) backend.
//!
//! The database is the durable store, shared by the bot and the web service.
//! Recent rows and the wiki list are mirrored in memory; the web UI reads that
//! mirror, so its handlers stay synchronous and cheap. The schema lives in
//! `schema.sql` and is embedded at compile time.

use anyhow::{Context, Result, anyhow, bail};

use std::collections::VecDeque;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

use toolforge::pool::ToolsDbPool;
use toolforge::pool::mysql_async::{Conn, Params, Row, Value, prelude::*};

use super::Cache;
use crate::config::{Config, StoreConfig};
use crate::mediawiki::Cursor;
use crate::model::{Detection, Level};
use crate::wiki::Wiki;

/// The database name when `store.database` is empty.
const DATABASE_SUFFIX: &str = "__faike_p";

/// The schema, embedded so `schema.sql` is the only copy to update.
const SCHEMA: &str = include_str!("../../schema.sql");

const INSERT: &str = "INSERT IGNORE INTO detections
    (wiki, id, title, timestamp, `user`, comment, url, description_url, mime, size, sha1, score, signals)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

const SELECT_RECENT: &str = "SELECT wiki, id, title, timestamp, `user`, comment, url,
    description_url, mime, size, sha1, score, signals
    FROM detections ORDER BY timestamp DESC, wiki DESC, id DESC LIMIT ?";

const SELECT_RECENT_FOR_WIKI: &str = "SELECT wiki, id, title, timestamp, `user`, comment, url,
    description_url, mime, size, sha1, score, signals
    FROM detections WHERE wiki = ? ORDER BY timestamp DESC, id DESC LIMIT ?";

const SELECT_WIKIS: &str = "SELECT domain, display_name, api_url, namespace, enabled,
    sort_order, cursor_timestamp, cursor_rc_id
    FROM wikis ORDER BY sort_order, domain";

const INSERT_WIKI: &str = "INSERT IGNORE INTO wikis
    (domain, display_name, api_url, namespace, enabled, sort_order)
    VALUES (?, ?, ?, ?, ?, ?)";

const UPSERT_CURSOR: &str = "UPDATE wikis SET cursor_timestamp = ?, cursor_rc_id = ?
    WHERE domain = ?";

const SEEDED: &str = "SELECT `value` FROM meta WHERE `key` = 'wikis_seeded'";

const MARK_SEEDED: &str = "INSERT INTO meta (`key`, `value`) VALUES ('wikis_seeded', '1')
    ON DUPLICATE KEY UPDATE `value` = `value`";

pub struct ToolsDbStore {
    pool: ToolsDbPool,
    database: String,
    cache: Cache,
    wikis: RwLock<Vec<Wiki>>,
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
        seed_wikis(&pool, &database).await?;
        // Seed from the primary: the schema was just ensured there, and the
        // read-only replica may lag on a fresh deployment.
        let mut conn = connect(&pool, &database, false).await?;
        let wikis = load_wikis(&mut conn).await?;
        let records = load_recent(&mut conn, config.max_records, &wikis).await?;
        drop(conn);
        let cache = Cache::new(config.max_records);
        cache.replace(records);
        Ok(Self {
            pool,
            database,
            cache,
            wikis: RwLock::new(wikis),
            read_only,
        })
    }

    /// Reload detections and the wiki list from the database.
    pub async fn reload(&self) -> Result<()> {
        let mut conn = self.connect().await?;
        let wikis = load_wikis(&mut conn).await?;
        let records = load_recent(&mut conn, self.cache.max_records(), &wikis).await?;
        drop(conn);
        self.cache.replace(records);
        *self.write_wikis() = wikis;
        Ok(())
    }

    /// Refresh only the wiki list and cursors, for the bot's per-poll reload.
    pub async fn refresh_wikis(&self) -> Result<()> {
        let mut conn = self.connect().await?;
        let wikis = load_wikis(&mut conn).await?;
        drop(conn);
        *self.write_wikis() = wikis;
        Ok(())
    }

    pub fn wikis(&self) -> Vec<Wiki> {
        self.read_wikis().clone()
    }

    pub fn is_seen(&self, wiki: &str, id: &str) -> bool {
        self.cache.is_seen(wiki, id)
    }

    pub fn mark_seen(&self, wiki: &str, id: &str) {
        self.cache.mark_seen(wiki, id);
    }

    /// Store a detection. Returns `false` if the (wiki, id) pair was already
    /// stored, which `INSERT IGNORE` also enforces across processes.
    pub async fn add(&self, detection: Detection) -> Result<bool> {
        if self.read_only {
            bail!("Detection store is read-only");
        }
        if self.cache.is_seen(&detection.wiki, &detection.id) {
            return Ok(false);
        }
        let signals =
            serde_json::to_string(&detection.signals).context("Serializing detection signals")?;
        let params = Params::Positional(vec![
            Value::from(detection.wiki.clone()),
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
            self.cache.mark_seen(&detection.wiki, &detection.id);
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

    pub async fn load_cursor(&self, wiki: &str) -> Option<Cursor> {
        self.read_wikis()
            .iter()
            .find(|entry| entry.domain == wiki)
            .and_then(|entry| entry.cursor.clone())
    }

    pub async fn save_cursor(&self, wiki: &str, cursor: &Cursor) -> Result<()> {
        if self.read_only {
            bail!("Detection store is read-only");
        }
        let mut conn = self.connect().await?;
        conn.exec_drop(
            UPSERT_CURSOR,
            (cursor.timestamp.as_str(), cursor.rc_id, wiki),
        )
        .await
        .with_context(|| format!("Saving the cursor to ToolsDB {}", self.database))?;
        {
            let mut wikis = self.write_wikis();
            if let Some(entry) = wikis.iter_mut().find(|entry| entry.domain == wiki) {
                entry.cursor = Some(cursor.clone());
            }
        }
        Ok(())
    }

    async fn connect(&self) -> Result<Conn> {
        connect(&self.pool, &self.database, self.read_only).await
    }

    fn read_wikis(&self) -> RwLockReadGuard<'_, Vec<Wiki>> {
        self.wikis
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_wikis(&self) -> RwLockWriteGuard<'_, Vec<Wiki>> {
        self.wikis
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    for statement in SCHEMA.split(';') {
        let statement = statement.trim();
        if statement.is_empty() {
            continue;
        }
        conn.query_drop(statement)
            .await
            .with_context(|| format!("Cannot apply schema statement to ToolsDB {database}"))?;
    }
    Ok(())
}

/// Insert the default wikis once. A `meta` flag means removing a wiki on
/// purpose is not undone by the next restart.
async fn seed_wikis(pool: &ToolsDbPool, database: &str) -> Result<()> {
    let mut conn = connect(pool, database, false).await?;
    if conn
        .query_first::<String, _>(SEEDED)
        .await
        .with_context(|| format!("Reading the wiki seed flag from ToolsDB {database}"))?
        .is_some()
    {
        return Ok(());
    }
    for wiki in Wiki::defaults() {
        conn.exec_drop(
            INSERT_WIKI,
            (
                wiki.domain.as_str(),
                wiki.display_name.as_str(),
                wiki.api_url.as_str(),
                wiki.namespace,
                wiki.enabled,
                wiki.sort_order,
            ),
        )
        .await
        .with_context(|| format!("Seeding wikis in ToolsDB {database}"))?;
    }
    conn.query_drop(MARK_SEEDED)
        .await
        .with_context(|| format!("Marking wikis as seeded in ToolsDB {database}"))?;
    Ok(())
}

/// Load at most `limit` recent rows per wiki, plus the global newest rows so
/// detections for removed wikis still appear in the "all" view.
async fn load_recent(conn: &mut Conn, limit: usize, wikis: &[Wiki]) -> Result<VecDeque<Detection>> {
    let mut rows = Vec::new();
    for wiki in wikis {
        let wiki_rows: Vec<Row> = conn
            .exec(SELECT_RECENT_FOR_WIKI, (wiki.domain.as_str(), limit))
            .await
            .with_context(|| format!("Reading recent detections for {}", wiki.domain))?;
        rows.extend(wiki_rows);
    }
    let global: Vec<Row> = conn
        .exec(SELECT_RECENT, (limit,))
        .await
        .context("Reading recent detections from ToolsDB")?;
    rows.extend(global);

    let mut seen = std::collections::HashSet::new();
    let mut detections: Vec<Detection> = rows
        .into_iter()
        .filter_map(detection_from_row)
        .filter(|detection| seen.insert((detection.wiki.clone(), detection.id.clone())))
        .collect();
    detections.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| b.id.cmp(&a.id))
            .then_with(|| b.wiki.cmp(&a.wiki))
    });
    Ok(detections.into())
}

async fn load_wikis(conn: &mut Conn) -> Result<Vec<Wiki>> {
    let rows: Vec<Row> = conn
        .query(SELECT_WIKIS)
        .await
        .context("Reading wikis from ToolsDB")?;
    Ok(rows.into_iter().filter_map(wiki_from_row).collect())
}

/// Rows written by an older schema or a partial insert are skipped rather
/// than failing the whole reload.
fn detection_from_row(row: Row) -> Option<Detection> {
    let score: u8 = row.get("score")?;
    let signals = serde_json::from_str(&row.get::<String, _>("signals")?).ok()?;
    Some(Detection {
        id: row.get("id")?,
        wiki: row.get("wiki")?,
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

fn wiki_from_row(row: Row) -> Option<Wiki> {
    let timestamp: String = row.get("cursor_timestamp")?;
    let rc_id: u64 = row.get("cursor_rc_id").unwrap_or(0);
    Some(Wiki {
        domain: row.get("domain")?,
        display_name: row.get("display_name")?,
        api_url: row.get("api_url")?,
        namespace: row.get("namespace")?,
        enabled: row.get("enabled")?,
        sort_order: row.get("sort_order")?,
        cursor: (!timestamp.is_empty()).then_some(Cursor { timestamp, rc_id }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::detection;

    const COMMONS: &str = "commons.wikimedia.org";

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

    #[test]
    fn schema_has_the_expected_tables() {
        for table in ["CREATE TABLE IF NOT EXISTS wikis", "detections", "meta"] {
            assert!(SCHEMA.contains(table), "missing {table}");
        }
    }

    /// Round-trip against the real ToolsDB. Opt in with:
    /// `FAIKE_TOOLSDB_TEST=1 cargo test -- --ignored`
    #[tokio::test]
    #[ignore = "needs the ToolsDB tunnel and credentials"]
    async fn round_trips_against_toolsdb() {
        if std::env::var_os("FAIKE_TOOLSDB_TEST").is_none() {
            return;
        }
        let my_cnf = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("replica.my.cnf");
        if !my_cnf.exists() {
            return;
        }
        let config = config("s58339__faike_p", my_cnf);
        let store = ToolsDbStore::open(&config).await.unwrap();
        assert!(store.wikis().iter().any(|wiki| wiki.domain == COMMONS));
        assert!(
            store
                .wikis()
                .iter()
                .any(|wiki| wiki.domain == "de.wikipedia.org")
        );
        let id = format!("faike-test-{}", std::process::id());
        let original_cursor = store.load_cursor(COMMONS).await;
        let sample = detection(COMMONS, &id, 90);
        assert!(store.add(sample.clone()).await.unwrap());
        assert!(!store.add(sample.clone()).await.unwrap(), "deduplicated");
        assert!(
            store
                .add(detection("en.wikipedia.org", &id, 90))
                .await
                .unwrap()
        );
        store.reload().await.unwrap();
        assert!(store.is_seen(COMMONS, &id));
        assert!(store.is_seen("en.wikipedia.org", &id));

        let cursor = Cursor {
            timestamp: "2026-01-01T00:00:00Z".into(),
            rc_id: 1,
        };
        store.save_cursor(COMMONS, &cursor).await.unwrap();
        assert_eq!(store.load_cursor(COMMONS).await.unwrap().rc_id, 1);

        let mut conn = store.connect().await.unwrap();
        conn.exec_drop("DELETE FROM detections WHERE id = ?", (id,))
            .await
            .unwrap();
        match &original_cursor {
            Some(cursor) => {
                conn.exec_drop(
                    UPSERT_CURSOR,
                    (cursor.timestamp.as_str(), cursor.rc_id, COMMONS),
                )
                .await
                .unwrap();
            }
            None => {
                conn.exec_drop(
                    "UPDATE wikis SET cursor_timestamp = '', cursor_rc_id = 0 WHERE domain = ?",
                    (COMMONS,),
                )
                .await
                .unwrap();
            }
        }
    }
}
