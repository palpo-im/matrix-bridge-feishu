use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use diesel::{
    prelude::*,
    r2d2::{ConnectionManager, Pool},
    sqlite::SqliteConnection,
};
use parking_lot::Mutex;

use super::error::{DatabaseError, DatabaseResult};
use super::models::{MessageMapping, ProcessedEvent, RoomMapping, UserMapping};
use super::stores::{EventStore, MessageStore, RoomStore, UserStore};

type SqlitePool = Pool<ConnectionManager<SqliteConnection>>;

table! {
    room_mappings (id) {
        id -> BigInt,
        matrix_room_id -> Text,
        feishu_chat_id -> Text,
        feishu_chat_name -> Nullable<Text>,
        feishu_chat_type -> Text,
        created_at -> Text,
        updated_at -> Text,
    }
}

table! {
    user_mappings (id) {
        id -> BigInt,
        matrix_user_id -> Text,
        feishu_user_id -> Text,
        feishu_username -> Nullable<Text>,
        feishu_avatar -> Nullable<Text>,
        created_at -> Text,
        updated_at -> Text,
    }
}

table! {
    message_mappings (id) {
        id -> BigInt,
        matrix_event_id -> Text,
        feishu_message_id -> Text,
        room_id -> Text,
        sender_mxid -> Text,
        sender_feishu_id -> Text,
        content_hash -> Nullable<Text>,
        created_at -> Text,
    }
}

table! {
    processed_events (id) {
        id -> BigInt,
        event_id -> Text,
        event_type -> Text,
        source -> Text,
        processed_at -> Text,
    }
}

#[derive(Clone)]
pub struct SqliteStores {
    pool: SqlitePool,
    room_cache: Arc<Mutex<lru::LruCache<String, RoomMapping>>>,
    user_cache: Arc<Mutex<lru::LruCache<String, UserMapping>>>,
}

impl SqliteStores {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            room_cache: Arc::new(Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(1000).unwrap()))),
            user_cache: Arc::new(Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(1000).unwrap()))),
        }
    }

    pub fn room_store(&self) -> Arc<dyn RoomStore> {
        Arc::new(self.clone())
    }

    pub fn user_store(&self) -> Arc<dyn UserStore> {
        Arc::new(self.clone())
    }

    pub fn message_store(&self) -> Arc<dyn MessageStore> {
        Arc::new(self.clone())
    }

    pub fn event_store(&self) -> Arc<dyn EventStore> {
        Arc::new(self.clone())
    }
}

#[async_trait]
impl RoomStore for SqliteStores {
    async fn get_room_by_matrix_id(&self, matrix_room_id: &str) -> DatabaseResult<Option<RoomMapping>> {
        let cache_key = format!("mx:{}", matrix_room_id);
        if let Some(cached) = self.room_cache.lock().get(&cache_key).cloned() {
            return Ok(Some(cached));
        }

        let pool = self.pool.clone();
        let room_id = matrix_room_id.to_string();
        let result: DatabaseResult<Option<RoomMapping>> = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let room: Option<SqliteRoomMapping> = room_mappings::table
                .filter(room_mappings::matrix_room_id.eq(&room_id))
                .first(&mut conn)
                .optional()
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(room.map(|r| r.into_model()))
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;

        if let Ok(Some(ref room)) = result {
            self.room_cache.lock().put(cache_key, room.clone());
        }
        result
    }

    async fn get_room_by_feishu_id(&self, feishu_chat_id: &str) -> DatabaseResult<Option<RoomMapping>> {
        let cache_key = format!("fs:{}", feishu_chat_id);
        if let Some(cached) = self.room_cache.lock().get(&cache_key).cloned() {
            return Ok(Some(cached));
        }

        let pool = self.pool.clone();
        let chat_id = feishu_chat_id.to_string();
        let result: DatabaseResult<Option<RoomMapping>> = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let room: Option<SqliteRoomMapping> = room_mappings::table
                .filter(room_mappings::feishu_chat_id.eq(&chat_id))
                .first(&mut conn)
                .optional()
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(room.map(|r| r.into_model()))
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;

        if let Ok(Some(ref room)) = result {
            self.room_cache.lock().put(cache_key, room.clone());
        }
        result
    }

    async fn create_room_mapping(&self, mapping: &RoomMapping) -> DatabaseResult<RoomMapping> {
        let pool = self.pool.clone();
        let mapping = mapping.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let sqlite_mapping = SqliteRoomMapping::from_model(&mapping);
            diesel::insert_into(room_mappings::table)
                .values(&sqlite_mapping)
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(mapping)
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn update_room_mapping(&self, mapping: &RoomMapping) -> DatabaseResult<()> {
        let pool = self.pool.clone();
        let mapping = mapping.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let sqlite_mapping = SqliteRoomMapping::from_model(&mapping);
            diesel::update(room_mappings::table.filter(room_mappings::id.eq(mapping.id)))
                .set(&sqlite_mapping)
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn delete_room_mapping(&self, id: i64) -> DatabaseResult<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            diesel::delete(room_mappings::table.filter(room_mappings::id.eq(id)))
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn list_room_mappings(&self, limit: Option<i64>, offset: Option<i64>) -> DatabaseResult<Vec<RoomMapping>> {
        let pool = self.pool.clone();
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let rooms: Vec<SqliteRoomMapping> = room_mappings::table
                .order(room_mappings::id.desc())
                .limit(limit)
                .offset(offset)
                .load(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(rooms.into_iter().map(|r| r.into_model()).collect())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn count_rooms(&self) -> DatabaseResult<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let count: i64 = room_mappings::table
                .count()
                .get_result(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(count)
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }
}

#[async_trait]
impl UserStore for SqliteStores {
    async fn get_user_by_matrix_id(&self, matrix_user_id: &str) -> DatabaseResult<Option<UserMapping>> {
        let cache_key = format!("mx:{}", matrix_user_id);
        if let Some(cached) = self.user_cache.lock().get(&cache_key).cloned() {
            return Ok(Some(cached));
        }

        let pool = self.pool.clone();
        let user_id = matrix_user_id.to_string();
        let result: DatabaseResult<Option<UserMapping>> = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let user: Option<SqliteUserMapping> = user_mappings::table
                .filter(user_mappings::matrix_user_id.eq(&user_id))
                .first(&mut conn)
                .optional()
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(user.map(|u| u.into_model()))
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;

        if let Ok(Some(ref user)) = result {
            self.user_cache.lock().put(cache_key, user.clone());
        }
        result
    }

    async fn get_user_by_feishu_id(&self, feishu_user_id: &str) -> DatabaseResult<Option<UserMapping>> {
        let cache_key = format!("fs:{}", feishu_user_id);
        if let Some(cached) = self.user_cache.lock().get(&cache_key).cloned() {
            return Ok(Some(cached));
        }

        let pool = self.pool.clone();
        let fs_user_id = feishu_user_id.to_string();
        let result: DatabaseResult<Option<UserMapping>> = tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let user: Option<SqliteUserMapping> = user_mappings::table
                .filter(user_mappings::feishu_user_id.eq(&fs_user_id))
                .first(&mut conn)
                .optional()
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(user.map(|u| u.into_model()))
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?;

        if let Ok(Some(ref user)) = result {
            self.user_cache.lock().put(cache_key, user.clone());
        }
        result
    }

    async fn create_user_mapping(&self, mapping: &UserMapping) -> DatabaseResult<UserMapping> {
        let pool = self.pool.clone();
        let mapping = mapping.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let sqlite_mapping = SqliteUserMapping::from_model(&mapping);
            diesel::insert_into(user_mappings::table)
                .values(&sqlite_mapping)
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(mapping)
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn update_user_mapping(&self, mapping: &UserMapping) -> DatabaseResult<()> {
        let pool = self.pool.clone();
        let mapping = mapping.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let sqlite_mapping = SqliteUserMapping::from_model(&mapping);
            diesel::update(user_mappings::table.filter(user_mappings::id.eq(mapping.id)))
                .set(&sqlite_mapping)
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn delete_user_mapping(&self, id: i64) -> DatabaseResult<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            diesel::delete(user_mappings::table.filter(user_mappings::id.eq(id)))
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn list_user_mappings(&self, limit: Option<i64>, offset: Option<i64>) -> DatabaseResult<Vec<UserMapping>> {
        let pool = self.pool.clone();
        let limit = limit.unwrap_or(100);
        let offset = offset.unwrap_or(0);
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let users: Vec<SqliteUserMapping> = user_mappings::table
                .order(user_mappings::id.desc())
                .limit(limit)
                .offset(offset)
                .load(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(users.into_iter().map(|u| u.into_model()).collect())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }
}

#[async_trait]
impl MessageStore for SqliteStores {
    async fn get_message_by_matrix_id(&self, matrix_event_id: &str) -> DatabaseResult<Option<MessageMapping>> {
        let pool = self.pool.clone();
        let event_id = matrix_event_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let msg: Option<SqliteMessageMapping> = message_mappings::table
                .filter(message_mappings::matrix_event_id.eq(&event_id))
                .first(&mut conn)
                .optional()
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(msg.map(|m| m.into_model()))
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn get_message_by_feishu_id(&self, feishu_message_id: &str) -> DatabaseResult<Option<MessageMapping>> {
        let pool = self.pool.clone();
        let fs_msg_id = feishu_message_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let msg: Option<SqliteMessageMapping> = message_mappings::table
                .filter(message_mappings::feishu_message_id.eq(&fs_msg_id))
                .first(&mut conn)
                .optional()
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(msg.map(|m| m.into_model()))
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn create_message_mapping(&self, mapping: &MessageMapping) -> DatabaseResult<MessageMapping> {
        let pool = self.pool.clone();
        let mapping = mapping.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let sqlite_mapping = SqliteMessageMapping::from_model(&mapping);
            diesel::insert_into(message_mappings::table)
                .values(&sqlite_mapping)
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(mapping)
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn delete_message_mapping(&self, id: i64) -> DatabaseResult<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            diesel::delete(message_mappings::table.filter(message_mappings::id.eq(id)))
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn get_messages_by_room(&self, room_id: &str, limit: Option<i64>) -> DatabaseResult<Vec<MessageMapping>> {
        let pool = self.pool.clone();
        let room = room_id.to_string();
        let limit = limit.unwrap_or(100);
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let msgs: Vec<SqliteMessageMapping> = message_mappings::table
                .filter(message_mappings::room_id.eq(&room))
                .order(message_mappings::id.desc())
                .limit(limit)
                .load(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(msgs.into_iter().map(|m| m.into_model()).collect())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }
}

#[async_trait]
impl EventStore for SqliteStores {
    async fn is_event_processed(&self, event_id: &str) -> DatabaseResult<bool> {
        let pool = self.pool.clone();
        let id = event_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let count: i64 = processed_events::table
                .filter(processed_events::event_id.eq(&id))
                .count()
                .get_result(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(count > 0)
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn mark_event_processed(&self, event: &ProcessedEvent) -> DatabaseResult<()> {
        let pool = self.pool.clone();
        let event = event.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let sqlite_event = SqliteProcessedEvent::from_model(&event);
            diesel::insert_into(processed_events::table)
                .values(&sqlite_event)
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(())
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }

    async fn cleanup_old_events(&self, before: DateTime<Utc>) -> DatabaseResult<u64> {
        let pool = self.pool.clone();
        let before_str = before.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().map_err(|e| DatabaseError::Pool(e.to_string()))?;
            let count = diesel::delete(processed_events::table.filter(processed_events::processed_at.lt(&before_str)))
                .execute(&mut conn)
                .map_err(DatabaseError::from)?;
            Ok::<_, DatabaseError>(count as u64)
        })
        .await
        .map_err(|e| DatabaseError::Query(e.to_string()))?
    }
}

#[derive(Queryable, Insertable, AsChangeset)]
#[diesel(table_name = room_mappings)]
struct SqliteRoomMapping {
    id: i64,
    matrix_room_id: String,
    feishu_chat_id: String,
    feishu_chat_name: Option<String>,
    feishu_chat_type: String,
    created_at: String,
    updated_at: String,
}

impl SqliteRoomMapping {
    fn from_model(model: &RoomMapping) -> Self {
        Self {
            id: model.id,
            matrix_room_id: model.matrix_room_id.clone(),
            feishu_chat_id: model.feishu_chat_id.clone(),
            feishu_chat_name: model.feishu_chat_name.clone(),
            feishu_chat_type: model.feishu_chat_type.clone(),
            created_at: model.created_at.to_rfc3339(),
            updated_at: model.updated_at.to_rfc3339(),
        }
    }

    fn into_model(self) -> RoomMapping {
        RoomMapping {
            id: self.id,
            matrix_room_id: self.matrix_room_id,
            feishu_chat_id: self.feishu_chat_id,
            feishu_chat_name: self.feishu_chat_name,
            feishu_chat_type: self.feishu_chat_type,
            created_at: DateTime::parse_from_rfc3339(&self.created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&self.updated_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

#[derive(Queryable, Insertable, AsChangeset)]
#[diesel(table_name = user_mappings)]
struct SqliteUserMapping {
    id: i64,
    matrix_user_id: String,
    feishu_user_id: String,
    feishu_username: Option<String>,
    feishu_avatar: Option<String>,
    created_at: String,
    updated_at: String,
}

impl SqliteUserMapping {
    fn from_model(model: &UserMapping) -> Self {
        Self {
            id: model.id,
            matrix_user_id: model.matrix_user_id.clone(),
            feishu_user_id: model.feishu_user_id.clone(),
            feishu_username: model.feishu_username.clone(),
            feishu_avatar: model.feishu_avatar.clone(),
            created_at: model.created_at.to_rfc3339(),
            updated_at: model.updated_at.to_rfc3339(),
        }
    }

    fn into_model(self) -> UserMapping {
        UserMapping {
            id: self.id,
            matrix_user_id: self.matrix_user_id,
            feishu_user_id: self.feishu_user_id,
            feishu_username: self.feishu_username,
            feishu_avatar: self.feishu_avatar,
            created_at: DateTime::parse_from_rfc3339(&self.created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            updated_at: DateTime::parse_from_rfc3339(&self.updated_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

#[derive(Queryable, Insertable)]
#[diesel(table_name = message_mappings)]
struct SqliteMessageMapping {
    id: i64,
    matrix_event_id: String,
    feishu_message_id: String,
    room_id: String,
    sender_mxid: String,
    sender_feishu_id: String,
    content_hash: Option<String>,
    created_at: String,
}

impl SqliteMessageMapping {
    fn from_model(model: &MessageMapping) -> Self {
        Self {
            id: model.id,
            matrix_event_id: model.matrix_event_id.clone(),
            feishu_message_id: model.feishu_message_id.clone(),
            room_id: model.room_id.clone(),
            sender_mxid: model.sender_mxid.clone(),
            sender_feishu_id: model.sender_feishu_id.clone(),
            content_hash: model.content_hash.clone(),
            created_at: model.created_at.to_rfc3339(),
        }
    }

    fn into_model(self) -> MessageMapping {
        MessageMapping {
            id: self.id,
            matrix_event_id: self.matrix_event_id,
            feishu_message_id: self.feishu_message_id,
            room_id: self.room_id,
            sender_mxid: self.sender_mxid,
            sender_feishu_id: self.sender_feishu_id,
            content_hash: self.content_hash,
            created_at: DateTime::parse_from_rfc3339(&self.created_at)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

#[derive(Queryable, Insertable)]
#[diesel(table_name = processed_events)]
struct SqliteProcessedEvent {
    id: i64,
    event_id: String,
    event_type: String,
    source: String,
    processed_at: String,
}

impl SqliteProcessedEvent {
    fn from_model(model: &ProcessedEvent) -> Self {
        Self {
            id: model.id,
            event_id: model.event_id.clone(),
            event_type: model.event_type.clone(),
            source: model.source.clone(),
            processed_at: model.processed_at.to_rfc3339(),
        }
    }
}
