use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::error::DatabaseResult;
use super::models::{MessageMapping, ProcessedEvent, RoomMapping, UserMapping};

#[async_trait]
pub trait RoomStore: Send + Sync {
    async fn get_room_by_matrix_id(
        &self,
        matrix_room_id: &str,
    ) -> DatabaseResult<Option<RoomMapping>>;
    async fn get_room_by_feishu_id(
        &self,
        feishu_chat_id: &str,
    ) -> DatabaseResult<Option<RoomMapping>>;
    async fn create_room_mapping(&self, mapping: &RoomMapping) -> DatabaseResult<RoomMapping>;
    async fn update_room_mapping(&self, mapping: &RoomMapping) -> DatabaseResult<()>;
    async fn delete_room_mapping(&self, id: i64) -> DatabaseResult<()>;
    async fn list_room_mappings(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> DatabaseResult<Vec<RoomMapping>>;
    async fn count_rooms(&self) -> DatabaseResult<i64>;
}

#[async_trait]
pub trait UserStore: Send + Sync {
    async fn get_user_by_matrix_id(
        &self,
        matrix_user_id: &str,
    ) -> DatabaseResult<Option<UserMapping>>;
    async fn get_user_by_feishu_id(
        &self,
        feishu_user_id: &str,
    ) -> DatabaseResult<Option<UserMapping>>;
    async fn create_user_mapping(&self, mapping: &UserMapping) -> DatabaseResult<UserMapping>;
    async fn update_user_mapping(&self, mapping: &UserMapping) -> DatabaseResult<()>;
    async fn delete_user_mapping(&self, id: i64) -> DatabaseResult<()>;
    async fn list_user_mappings(
        &self,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> DatabaseResult<Vec<UserMapping>>;
}

#[async_trait]
pub trait MessageStore: Send + Sync {
    async fn get_message_by_matrix_id(
        &self,
        matrix_event_id: &str,
    ) -> DatabaseResult<Option<MessageMapping>>;
    async fn get_message_by_feishu_id(
        &self,
        feishu_message_id: &str,
    ) -> DatabaseResult<Option<MessageMapping>>;
    async fn create_message_mapping(
        &self,
        mapping: &MessageMapping,
    ) -> DatabaseResult<MessageMapping>;
    async fn delete_message_mapping(&self, id: i64) -> DatabaseResult<()>;
    async fn get_messages_by_room(
        &self,
        room_id: &str,
        limit: Option<i64>,
    ) -> DatabaseResult<Vec<MessageMapping>>;
}

#[async_trait]
pub trait EventStore: Send + Sync {
    async fn is_event_processed(&self, event_id: &str) -> DatabaseResult<bool>;
    async fn mark_event_processed(&self, event: &ProcessedEvent) -> DatabaseResult<()>;
    async fn cleanup_old_events(&self, before: DateTime<Utc>) -> DatabaseResult<u64>;
}

pub type SharedRoomStore = Arc<dyn RoomStore>;
pub type SharedUserStore = Arc<dyn UserStore>;
pub type SharedMessageStore = Arc<dyn MessageStore>;
pub type SharedEventStore = Arc<dyn EventStore>;
