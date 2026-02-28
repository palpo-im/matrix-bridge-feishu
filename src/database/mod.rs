pub mod error;
pub mod models;
pub mod sqlite_stores;
pub mod stores;

pub use error::{DatabaseError, DatabaseResult};
pub use models::{MessageMapping, ProcessedEvent, RoomMapping, UserMapping};
pub use stores::{EventStore, MessageStore, RoomStore, UserStore};

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::pg::PgConnection;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::sqlite::SqliteConnection;
use tracing::info;

pub type SqlitePool = Pool<ConnectionManager<SqliteConnection>>;
pub type PgPool = Pool<ConnectionManager<PgConnection>>;

#[derive(Clone)]
pub enum DatabasePool {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

#[derive(Clone)]
pub struct Database {
    pool: DatabasePool,
    db_type: String,
}

impl Database {
    pub async fn connect(
        db_type: &str,
        db_uri: &str,
        max_open: u32,
        max_idle: u32,
    ) -> Result<Self> {
        info!("Connecting to {} database: {}", db_type, db_uri);

        let db_kind = db_type.trim().to_ascii_lowercase();
        let max_size = max_open.max(1);
        let min_idle = Some(max_idle.min(max_size));

        let pool = match db_kind.as_str() {
            "sqlite" => {
                let db_path = sqlite_path_from_uri(db_uri)?;
                let is_memory = db_path == Path::new(":memory:");
                let db_existed = is_memory || db_path.exists();

                if !is_memory {
                    if let Some(parent) = db_path.parent() {
                        if !parent.as_os_str().is_empty() {
                            std::fs::create_dir_all(parent)?;
                        }
                    }
                }

                let db_url = db_path.to_string_lossy().to_string();
                let pool = tokio::task::spawn_blocking(move || -> Result<SqlitePool> {
                    let manager = ConnectionManager::<SqliteConnection>::new(db_url);
                    let pool = Pool::builder()
                        .max_size(max_size)
                        .min_idle(min_idle)
                        .build(manager)?;
                    Ok(pool)
                })
                .await
                .context("sqlite pool init task panicked")??;

                if !db_existed {
                    info!("Created new {} database", db_type);
                }

                DatabasePool::Sqlite(pool)
            }
            "postgres" | "postgresql" | "pgsql" => {
                let db_url = db_uri.to_string();
                let pool = tokio::task::spawn_blocking(move || -> Result<PgPool> {
                    let manager = ConnectionManager::<PgConnection>::new(db_url);
                    let pool = Pool::builder()
                        .max_size(max_size)
                        .min_idle(min_idle)
                        .build(manager)?;
                    Ok(pool)
                })
                .await
                .context("postgres pool init task panicked")??;

                DatabasePool::Postgres(pool)
            }
            _ => {
                anyhow::bail!(
                    "database type '{}' is not supported; use sqlite or postgres",
                    db_type
                );
            }
        };

        Ok(Self {
            pool,
            db_type: db_kind,
        })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        info!("Running database migrations for {}", self.db_type);

        match self.pool.clone() {
            DatabasePool::Sqlite(pool) => {
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut conn = pool.get()?;
                    conn.batch_execute(SQLITE_MIGRATIONS)?;
                    Ok(())
                })
                .await
                .context("sqlite migration task panicked")??;
            }
            DatabasePool::Postgres(pool) => {
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut conn = pool.get()?;
                    conn.batch_execute(POSTGRES_MIGRATIONS)?;
                    Ok(())
                })
                .await
                .context("postgres migration task panicked")??;
            }
        }

        info!("Database migrations completed");
        Ok(())
    }

    pub fn get_pool(&self) -> &DatabasePool {
        &self.pool
    }
}

fn sqlite_path_from_uri(db_uri: &str) -> Result<PathBuf> {
    if db_uri.is_empty() {
        anyhow::bail!("database uri cannot be empty");
    }

    let path = db_uri
        .strip_prefix("sqlite://")
        .or_else(|| db_uri.strip_prefix("sqlite:"))
        .unwrap_or(db_uri);

    if path.is_empty() {
        anyhow::bail!("database uri '{}' does not contain a sqlite path", db_uri);
    }

    Ok(PathBuf::from(path))
}

const SQLITE_MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    mxid TEXT NOT NULL UNIQUE,
    feishu_user_id TEXT,
    feishu_token TEXT,
    is_whitelisted BOOLEAN NOT NULL DEFAULT FALSE,
    is_admin BOOLEAN NOT NULL DEFAULT FALSE,
    relay_bot TEXT,
    management_room TEXT,
    space_room TEXT,
    timezone TEXT,
    connection_state TEXT NOT NULL DEFAULT 'disconnected',
    next_batch TEXT
);

CREATE TABLE IF NOT EXISTS puppets (
    id INTEGER PRIMARY KEY,
    feishu_id TEXT NOT NULL UNIQUE,
    mxid TEXT NOT NULL UNIQUE,
    displayname TEXT NOT NULL,
    avatar_url TEXT,
    access_token TEXT,
    custom_mxid TEXT,
    next_batch TEXT,
    base_url TEXT,
    is_online BOOLEAN NOT NULL DEFAULT FALSE,
    name_set BOOLEAN NOT NULL DEFAULT FALSE,
    avatar_set BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS portals (
    id INTEGER PRIMARY KEY,
    feishu_room_id TEXT NOT NULL UNIQUE,
    mxid TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    topic TEXT,
    avatar_url TEXT,
    encrypted BOOLEAN NOT NULL DEFAULT FALSE,
    room_type TEXT NOT NULL DEFAULT 'group',
    relay_user_id TEXT,
    creator_mxid TEXT NOT NULL,
    bridge_info TEXT NOT NULL,
    creation_content TEXT,
    last_event TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    mxid TEXT NOT NULL UNIQUE,
    feishu_message_id TEXT NOT NULL,
    room_id TEXT NOT NULL,
    sender TEXT NOT NULL,
    content TEXT NOT NULL,
    msg_type TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    attachments TEXT
);

CREATE TABLE IF NOT EXISTS room_mappings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    matrix_room_id TEXT NOT NULL UNIQUE,
    feishu_chat_id TEXT NOT NULL UNIQUE,
    feishu_chat_name TEXT,
    feishu_chat_type TEXT NOT NULL DEFAULT 'group',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS user_mappings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    matrix_user_id TEXT NOT NULL UNIQUE,
    feishu_user_id TEXT NOT NULL UNIQUE,
    feishu_username TEXT,
    feishu_avatar TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS message_mappings (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    matrix_event_id TEXT NOT NULL UNIQUE,
    feishu_message_id TEXT NOT NULL UNIQUE,
    thread_id TEXT,
    root_id TEXT,
    parent_id TEXT,
    room_id TEXT NOT NULL,
    sender_mxid TEXT NOT NULL,
    sender_feishu_id TEXT NOT NULL,
    content_hash TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS processed_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    source TEXT NOT NULL,
    processed_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_room_mappings_matrix_id ON room_mappings(matrix_room_id);
CREATE INDEX IF NOT EXISTS idx_room_mappings_feishu_id ON room_mappings(feishu_chat_id);
CREATE INDEX IF NOT EXISTS idx_user_mappings_matrix_id ON user_mappings(matrix_user_id);
CREATE INDEX IF NOT EXISTS idx_user_mappings_feishu_id ON user_mappings(feishu_user_id);
CREATE INDEX IF NOT EXISTS idx_message_mappings_matrix_id ON message_mappings(matrix_event_id);
CREATE INDEX IF NOT EXISTS idx_message_mappings_feishu_id ON message_mappings(feishu_message_id);
CREATE INDEX IF NOT EXISTS idx_message_mappings_room ON message_mappings(room_id);
CREATE INDEX IF NOT EXISTS idx_processed_events_event_id ON processed_events(event_id);
ALTER TABLE message_mappings ADD COLUMN IF NOT EXISTS thread_id TEXT;
ALTER TABLE message_mappings ADD COLUMN IF NOT EXISTS root_id TEXT;
ALTER TABLE message_mappings ADD COLUMN IF NOT EXISTS parent_id TEXT;
"#;

const POSTGRES_MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id BIGSERIAL PRIMARY KEY,
    mxid TEXT NOT NULL UNIQUE,
    feishu_user_id TEXT,
    feishu_token TEXT,
    is_whitelisted BOOLEAN NOT NULL DEFAULT FALSE,
    is_admin BOOLEAN NOT NULL DEFAULT FALSE,
    relay_bot TEXT,
    management_room TEXT,
    space_room TEXT,
    timezone TEXT,
    connection_state TEXT NOT NULL DEFAULT 'disconnected',
    next_batch TEXT
);

CREATE TABLE IF NOT EXISTS puppets (
    id BIGSERIAL PRIMARY KEY,
    feishu_id TEXT NOT NULL UNIQUE,
    mxid TEXT NOT NULL UNIQUE,
    displayname TEXT NOT NULL,
    avatar_url TEXT,
    access_token TEXT,
    custom_mxid TEXT,
    next_batch TEXT,
    base_url TEXT,
    is_online BOOLEAN NOT NULL DEFAULT FALSE,
    name_set BOOLEAN NOT NULL DEFAULT FALSE,
    avatar_set BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS portals (
    id BIGSERIAL PRIMARY KEY,
    feishu_room_id TEXT NOT NULL UNIQUE,
    mxid TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    topic TEXT,
    avatar_url TEXT,
    encrypted BOOLEAN NOT NULL DEFAULT FALSE,
    room_type TEXT NOT NULL DEFAULT 'group',
    relay_user_id TEXT,
    creator_mxid TEXT NOT NULL,
    bridge_info TEXT NOT NULL,
    creation_content TEXT,
    last_event TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id BIGSERIAL PRIMARY KEY,
    mxid TEXT NOT NULL UNIQUE,
    feishu_message_id TEXT NOT NULL,
    room_id TEXT NOT NULL,
    sender TEXT NOT NULL,
    content TEXT NOT NULL,
    msg_type TEXT NOT NULL,
    timestamp BIGINT NOT NULL,
    attachments TEXT
);

CREATE TABLE IF NOT EXISTS room_mappings (
    id BIGSERIAL PRIMARY KEY,
    matrix_room_id TEXT NOT NULL UNIQUE,
    feishu_chat_id TEXT NOT NULL UNIQUE,
    feishu_chat_name TEXT,
    feishu_chat_type TEXT NOT NULL DEFAULT 'group',
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS user_mappings (
    id BIGSERIAL PRIMARY KEY,
    matrix_user_id TEXT NOT NULL UNIQUE,
    feishu_user_id TEXT NOT NULL UNIQUE,
    feishu_username TEXT,
    feishu_avatar TEXT,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS message_mappings (
    id BIGSERIAL PRIMARY KEY,
    matrix_event_id TEXT NOT NULL UNIQUE,
    feishu_message_id TEXT NOT NULL UNIQUE,
    thread_id TEXT,
    root_id TEXT,
    parent_id TEXT,
    room_id TEXT NOT NULL,
    sender_mxid TEXT NOT NULL,
    sender_feishu_id TEXT NOT NULL,
    content_hash TEXT,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS processed_events (
    id BIGSERIAL PRIMARY KEY,
    event_id TEXT NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    source TEXT NOT NULL,
    processed_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_room_mappings_matrix_id ON room_mappings(matrix_room_id);
CREATE INDEX IF NOT EXISTS idx_room_mappings_feishu_id ON room_mappings(feishu_chat_id);
CREATE INDEX IF NOT EXISTS idx_user_mappings_matrix_id ON user_mappings(matrix_user_id);
CREATE INDEX IF NOT EXISTS idx_user_mappings_feishu_id ON user_mappings(feishu_user_id);
CREATE INDEX IF NOT EXISTS idx_message_mappings_matrix_id ON message_mappings(matrix_event_id);
CREATE INDEX IF NOT EXISTS idx_message_mappings_feishu_id ON message_mappings(feishu_message_id);
CREATE INDEX IF NOT EXISTS idx_message_mappings_room ON message_mappings(room_id);
CREATE INDEX IF NOT EXISTS idx_processed_events_event_id ON processed_events(event_id);
ALTER TABLE message_mappings ADD COLUMN IF NOT EXISTS thread_id TEXT;
ALTER TABLE message_mappings ADD COLUMN IF NOT EXISTS root_id TEXT;
ALTER TABLE message_mappings ADD COLUMN IF NOT EXISTS parent_id TEXT;
"#;
