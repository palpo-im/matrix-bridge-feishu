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
"#;
