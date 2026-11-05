-- fAIke / faike database schema.
--
-- This file is embedded by `src/store/toolsdb.rs` and executed on startup, so
-- it is the single source of truth for the tables on ToolsDB. Keep it valid
-- MariaDB 10.6 and avoid semicolons inside comments.
--
-- The `wikis` table is meant to be edited live:
--   INSERT INTO wikis (domain, display_name, api_url, sort_order) VALUES (...);
--   UPDATE wikis SET enabled = 0 WHERE domain = 'de.wikipedia.org';
--   DELETE FROM wikis WHERE domain = '...';
-- The bot refreshes it every poll, the web service every five seconds.

CREATE TABLE IF NOT EXISTS wikis (
    domain VARCHAR(255) NOT NULL,
    display_name VARCHAR(255) NOT NULL,
    api_url VARCHAR(255) NOT NULL,
    namespace INT NOT NULL DEFAULT 6,
    enabled TINYINT(1) NOT NULL DEFAULT 1,
    sort_order INT NOT NULL DEFAULT 0,
    cursor_timestamp VARCHAR(32) NOT NULL DEFAULT '',
    cursor_rc_id BIGINT UNSIGNED NOT NULL DEFAULT 0,
    updated TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
    PRIMARY KEY (domain)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS detections (
    wiki VARCHAR(255) NOT NULL,
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
    PRIMARY KEY (wiki, id),
    KEY idx_timestamp (timestamp),
    KEY idx_score_timestamp (score, timestamp),
    KEY idx_sha1 (sha1),
    KEY idx_wiki_timestamp (wiki, timestamp)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;

CREATE TABLE IF NOT EXISTS meta (
    `key` VARCHAR(64) NOT NULL,
    `value` MEDIUMTEXT NOT NULL,
    PRIMARY KEY (`key`)
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;
