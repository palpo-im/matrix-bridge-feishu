use salvo::prelude::*;

#[handler]
pub async fn health_endpoint(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render(Json(serde_json::json!({
        "status": "ok",
        "timestamp": chrono::Utc::now().to_rfc3339()
    })));
}

#[handler]
pub async fn ready_endpoint(res: &mut Response) {
    res.status_code(StatusCode::OK);
    res.render(Json(serde_json::json!({
        "ready": true
    })));
}
