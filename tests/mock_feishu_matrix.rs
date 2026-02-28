use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use diesel::r2d2::{ConnectionManager, Pool};
use diesel::sqlite::SqliteConnection;
use matrix_bridge_feishu::bridge::{MatrixEvent, MatrixEventProcessor, MessageFlow};
use matrix_bridge_feishu::config::{
    AppServiceConfig, BotConfig, BridgeConfig, Config, DatabaseConfig, HomeserverConfig,
    LoggingConfig, LoggingWriterConfig,
};
use matrix_bridge_feishu::database::sqlite_stores::SqliteStores;
use matrix_bridge_feishu::database::{Database, RoomMapping};
use matrix_bridge_feishu::feishu::FeishuService;
use salvo::affix_state;
use salvo::prelude::*;
use serde_json::json;
use uuid::Uuid;

#[derive(Clone)]
struct FeishuMockState {
    create_calls: Arc<AtomicU64>,
    upload_image_calls: Arc<AtomicU64>,
    update_calls: Arc<AtomicU64>,
    recall_calls: Arc<AtomicU64>,
}

#[derive(Clone)]
struct MatrixMockState {
    media_download_calls: Arc<AtomicU64>,
}

#[tokio::test]
async fn matrix_to_feishu_pipeline_with_mock_servers_persists_mapping() {
    let prev_no_proxy = std::env::var("NO_PROXY").ok();
    let prev_no_proxy_lower = std::env::var("no_proxy").ok();
    std::env::set_var("NO_PROXY", "127.0.0.1,localhost");
    std::env::set_var("no_proxy", "127.0.0.1,localhost");

    let feishu_state = FeishuMockState {
        create_calls: Arc::new(AtomicU64::new(0)),
        upload_image_calls: Arc::new(AtomicU64::new(0)),
        update_calls: Arc::new(AtomicU64::new(0)),
        recall_calls: Arc::new(AtomicU64::new(0)),
    };
    let (feishu_base, _feishu_handle) = start_feishu_mock(feishu_state.clone()).await;

    let matrix_state = MatrixMockState {
        media_download_calls: Arc::new(AtomicU64::new(0)),
    };
    let (matrix_base, _matrix_handle) = start_matrix_mock(matrix_state.clone()).await;
    wait_for_http_ready(&format!(
        "{}/open-apis/auth/v3/tenant_access_token/internal",
        feishu_base
    ))
    .await;
    wait_for_http_ready(&format!("{}/media/cat.png", matrix_base)).await;

    let db_path = std::env::temp_dir().join(format!("matrix-bridge-test-{}.db", Uuid::new_v4()));
    let db_uri = format!("sqlite:{}", db_path.to_string_lossy());

    let db = Database::connect("sqlite", &db_uri, 4, 1)
        .await
        .expect("db connect should succeed");
    db.run_migrations()
        .await
        .expect("migrations should succeed");

    let manager = ConnectionManager::<SqliteConnection>::new(db_path.to_string_lossy().to_string());
    let pool = Pool::builder()
        .max_size(4)
        .build(manager)
        .expect("pool should build");
    let stores = SqliteStores::new(pool);

    std::env::set_var("FEISHU_API_BASE_URL", format!("{}/open-apis", feishu_base));
    let config = Arc::new(build_test_config(&matrix_base, &db_uri));
    let feishu_service = Arc::new(FeishuService::new(
        "cli_app".to_string(),
        "cli_secret".to_string(),
        "127.0.0.1:38081".to_string(),
        "listen_secret".to_string(),
        None,
        None,
    ));

    let room_store = stores.room_store();
    room_store
        .create_room_mapping(&RoomMapping::new(
            "!room:localhost".to_string(),
            "oc_mock_chat".to_string(),
            Some("Mock Chat".to_string()),
        ))
        .await
        .expect("room mapping should be created");
    assert!(
        room_store
            .get_room_by_matrix_id("!room:localhost")
            .await
            .expect("room lookup should succeed")
            .is_some(),
        "room mapping should be queryable before processing"
    );

    let message_flow = Arc::new(MessageFlow::new(config.clone(), feishu_service.clone()));
    let processor = MatrixEventProcessor::new(
        config.clone(),
        feishu_service.clone(),
        stores.room_store(),
        stores.message_store(),
        stores.event_store(),
        stores.media_store(),
        message_flow,
    );

    let event = MatrixEvent {
        event_id: Some("$evt1".to_string()),
        event_type: "m.room.message".to_string(),
        room_id: "!room:localhost".to_string(),
        sender: "@alice:localhost".to_string(),
        state_key: None,
        content: Some(json!({
            "msgtype": "m.image",
            "body": "",
            "url": format!("{}/media/cat.png", matrix_base),
        })),
        timestamp: None,
    };
    let parsed =
        MessageFlow::parse_matrix_event("m.room.message", event.content.as_ref().expect("content"))
            .expect("matrix event should parse into outbound payload");
    assert_eq!(
        parsed.attachments.len(),
        1,
        "matrix image event should produce one attachment"
    );

    processor
        .process_event(event)
        .await
        .expect("event should process successfully");

    assert!(
        stores
            .event_store()
            .is_event_processed("$evt1")
            .await
            .expect("event lookup should succeed"),
        "matrix event should be marked processed"
    );
    assert!(
        stores
            .room_store()
            .get_room_by_matrix_id("!room:localhost")
            .await
            .expect("room lookup should succeed")
            .is_some(),
        "room mapping should still exist after processing"
    );

    let message_store = stores.message_store();
    let mapping_after_create = message_store
        .get_message_by_matrix_id("$evt1")
        .await
        .expect("query should succeed");
    assert!(
        mapping_after_create.is_some(),
        "mapping should exist after create"
    );
    assert!(
        feishu_state.create_calls.load(Ordering::Relaxed) >= 1,
        "expected at least one Feishu create call"
    );
    assert!(
        feishu_state.upload_image_calls.load(Ordering::Relaxed) >= 1,
        "expected at least one Feishu image upload call"
    );
    assert!(
        matrix_state.media_download_calls.load(Ordering::Relaxed) >= 1,
        "expected at least one Matrix media download call"
    );

    let edit_event = MatrixEvent {
        event_id: Some("$evt2".to_string()),
        event_type: "m.room.message".to_string(),
        room_id: "!room:localhost".to_string(),
        sender: "@alice:localhost".to_string(),
        state_key: None,
        content: Some(json!({
            "msgtype": "m.text",
            "body": "* updated body",
            "m.relates_to": {
                "rel_type": "m.replace",
                "event_id": "$evt1"
            },
            "m.new_content": {
                "msgtype": "m.text",
                "body": "updated body"
            }
        })),
        timestamp: None,
    };
    processor
        .process_event(edit_event)
        .await
        .expect("edit event should process");
    assert!(
        feishu_state.update_calls.load(Ordering::Relaxed) >= 1,
        "expected at least one Feishu update call"
    );

    let redaction_event = MatrixEvent {
        event_id: Some("$evt3".to_string()),
        event_type: "m.room.redaction".to_string(),
        room_id: "!room:localhost".to_string(),
        sender: "@alice:localhost".to_string(),
        state_key: None,
        content: Some(json!({ "redacts": "$evt1" })),
        timestamp: None,
    };
    processor
        .process_event(redaction_event)
        .await
        .expect("redaction event should process");
    assert!(
        feishu_state.recall_calls.load(Ordering::Relaxed) >= 1,
        "expected at least one Feishu recall call"
    );
    let mapping_after_redaction = message_store
        .get_message_by_matrix_id("$evt1")
        .await
        .expect("query should succeed");
    assert!(
        mapping_after_redaction.is_none(),
        "mapping should be removed after redaction"
    );

    std::env::remove_var("FEISHU_API_BASE_URL");
    if let Some(value) = prev_no_proxy {
        std::env::set_var("NO_PROXY", value);
    } else {
        std::env::remove_var("NO_PROXY");
    }
    if let Some(value) = prev_no_proxy_lower {
        std::env::set_var("no_proxy", value);
    } else {
        std::env::remove_var("no_proxy");
    }
    let _ = std::fs::remove_file(db_path);
}

fn build_test_config(matrix_base: &str, db_uri: &str) -> Config {
    let mut permissions = HashMap::new();
    permissions.insert("*".to_string(), "relay".to_string());
    permissions.insert("localhost".to_string(), "full".to_string());

    Config {
        homeserver: HomeserverConfig {
            address: matrix_base.to_string(),
            domain: "localhost".to_string(),
            software: "standard".to_string(),
            status_endpoint: None,
            message_send_checkpoint_endpoint: None,
            async_media: false,
            websocket: false,
            ping_interval_seconds: 0,
        },
        appservice: AppServiceConfig {
            address: "http://127.0.0.1:39080".to_string(),
            hostname: "127.0.0.1".to_string(),
            port: 39080,
            database: DatabaseConfig {
                r#type: "sqlite".to_string(),
                uri: db_uri.to_string(),
                max_open_conns: 4,
                max_idle_conns: 1,
                max_conn_idle_time: None,
                max_conn_lifetime: None,
            },
            id: "feishu-test".to_string(),
            bot: BotConfig {
                username: "feishubot".to_string(),
                displayname: "Feishu Test Bot".to_string(),
                avatar: "mxc://localhost/feishubot".to_string(),
            },
            ephemeral_events: false,
            async_transactions: false,
            as_token: "as_token_test_value".to_string(),
            hs_token: "hs_token_test_value".to_string(),
        },
        bridge: BridgeConfig {
            listen_address: "http://127.0.0.1:38081".to_string(),
            listen_secret: "listen_secret".to_string(),
            app_id: "cli_app".to_string(),
            app_secret: "cli_secret".to_string(),
            encrypt_key: None,
            verification_token: None,
            username_template: "feishu_{{.}}".to_string(),
            permissions,
            displayname_template: "{{.}} (Feishu)".to_string(),
            avatar_template: "".to_string(),
            bridge_matrix_reply: true,
            bridge_matrix_edit: true,
            bridge_matrix_reactions: false,
            bridge_matrix_redactions: true,
            bridge_matrix_leave: true,
            bridge_feishu_join: true,
            bridge_feishu_leave: true,
            allow_plain_text: true,
            allow_markdown: true,
            allow_html: false,
            allow_images: true,
            allow_videos: true,
            allow_audio: true,
            allow_files: true,
            max_media_size: 10 * 1024 * 1024,
            message_limit: 60,
            message_cooldown: 1000,
            blocked_matrix_msgtypes: Vec::new(),
            max_text_length: 0,
            enable_failure_degrade: true,
            failure_notice_template:
                "[bridge degraded] failed to deliver message from Matrix event {matrix_event_id}: {error}"
                    .to_string(),
            user_sync_interval_secs: 300,
            user_mapping_stale_ttl_hours: 720,
            webhook_timeout: 30,
            api_timeout: 60,
            enable_rich_text: true,
            convert_cards: true,
        },
        logging: LoggingConfig {
            min_level: "info".to_string(),
            writers: vec![LoggingWriterConfig {
                r#type: "stdout".to_string(),
                format: "pretty".to_string(),
                filename: None,
                max_size: None,
                max_backups: None,
                compress: None,
            }],
        },
    }
}

async fn start_feishu_mock(state: FeishuMockState) -> (String, tokio::task::JoinHandle<()>) {
    #[handler]
    async fn auth_handler(res: &mut Response) {
        res.render(Json(json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "tenant_access_token": "mock_token",
                "expire": 7200
            }
        })));
    }

    #[handler]
    async fn create_message_handler(depot: &mut Depot, res: &mut Response) {
        let state: &FeishuMockState = depot.obtain().expect("mock state should exist");
        state.create_calls.fetch_add(1, Ordering::Relaxed);
        res.render(Json(json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "message_id": "om_mock",
                "root_id": null,
                "parent_id": null,
                "thread_id": null
            }
        })));
    }

    #[handler]
    async fn update_message_handler(depot: &mut Depot, res: &mut Response) {
        let state: &FeishuMockState = depot.obtain().expect("mock state should exist");
        state.update_calls.fetch_add(1, Ordering::Relaxed);
        res.render(Json(json!({
            "code": 0,
            "msg": "ok",
            "data": {
                "message_id": "om_mock",
                "root_id": null,
                "parent_id": null,
                "thread_id": null
            }
        })));
    }

    #[handler]
    async fn recall_message_handler(depot: &mut Depot, res: &mut Response) {
        let state: &FeishuMockState = depot.obtain().expect("mock state should exist");
        state.recall_calls.fetch_add(1, Ordering::Relaxed);
        res.render(Json(json!({
            "code": 0,
            "msg": "ok",
            "data": {}
        })));
    }

    #[handler]
    async fn upload_image_handler(depot: &mut Depot, res: &mut Response) {
        let state: &FeishuMockState = depot.obtain().expect("mock state should exist");
        state.upload_image_calls.fetch_add(1, Ordering::Relaxed);
        res.render(Json(json!({
            "code": 0,
            "msg": "ok",
            "data": { "image_key": "img_key_mock" }
        })));
    }

    let router = Router::new()
        .hoop(affix_state::inject(state))
        .push(
            Router::with_path("open-apis/auth/v3/tenant_access_token/internal").post(auth_handler),
        )
        .push(Router::with_path("open-apis/im/v1/messages").post(create_message_handler))
        .push(Router::with_path("open-apis/im/v1/images").post(upload_image_handler))
        .push(Router::with_path("open-apis/im/v1/messages/om_mock").put(update_message_handler))
        .push(Router::with_path("open-apis/im/v1/messages/om_mock").delete(recall_message_handler));

    start_router(router).await
}

async fn start_matrix_mock(state: MatrixMockState) -> (String, tokio::task::JoinHandle<()>) {
    #[handler]
    async fn media_handler(depot: &mut Depot, res: &mut Response) {
        let state: &MatrixMockState = depot.obtain().expect("mock state should exist");
        state.media_download_calls.fetch_add(1, Ordering::Relaxed);
        res.status_code(StatusCode::OK);
        res.render("mock_media_payload");
    }

    let router = Router::new()
        .hoop(affix_state::inject(state))
        .push(Router::with_path("media/cat.png").get(media_handler));

    start_router(router).await
}

async fn start_router(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test port");
    let addr = probe.local_addr().expect("local addr");
    drop(probe);

    let acceptor = TcpListener::new(format!("127.0.0.1:{}", addr.port()))
        .bind()
        .await;
    let handle = tokio::spawn(async move {
        Server::new(acceptor).serve(router).await;
    });
    (format!("http://127.0.0.1:{}", addr.port()), handle)
}

async fn wait_for_http_ready(url: &str) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client should build");
    for _ in 0..20 {
        if client.get(url).send().await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
