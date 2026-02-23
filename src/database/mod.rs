use anyhow::Result;
use sqlx::{migrate::MigrateDatabase, SqlitePool};
use tracing::info;

pub struct Database {
    pool: SqlitePool,
    db_type: String,
}

impl Database {
    pub async fn connect(
        db_type: &str,
        db_uri: &str,
        _max_open: u32,
        _max_idle: u32,
    ) -> Result<Self> {
        info!("Connecting to {} database: {}", db_type, db_uri);

        if !db_type.eq_ignore_ascii_case("sqlite") {
            anyhow::bail!(
                "database type '{}' is not supported yet; use sqlite",
                db_type
            );
        }

        if !sqlx::Sqlite::database_exists(db_uri).await? {
            sqlx::Sqlite::create_database(db_uri).await?;
            info!("Created new {} database", db_type);
        }

        let pool = SqlitePool::connect(db_uri).await?;

        Ok(Self {
            pool,
            db_type: db_type.to_string(),
        })
    }

    pub async fn run_migrations(&self) -> Result<()> {
        info!("Running database migrations");

        // Create tables similar to DingTalk bridge
        sqlx::query(
            r#"
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
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
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
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
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
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
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
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        info!("Database migrations completed");
        Ok(())
    }

    pub fn get_pool(&self) -> &SqlitePool {
        &self.pool
    }
}

impl Clone for Database {
    fn clone(&self) -> Self {
        Self {
            pool: self.pool.clone(),
            db_type: self.db_type.clone(),
        }
    }
}
