use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};

/// List all environment variables (names only, no values).
pub async fn env_list(State(state): State<crate::server::AppState>) -> impl IntoResponse {
    let Some(ref store) = state.gateway.credential_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no credential store" })),
        )
            .into_response();
    };
    match store.list_env_vars().await {
        Ok(vars) => Json(serde_json::json!({ "env_vars": vars })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Set (upsert) an environment variable.
pub async fn env_set(
    State(state): State<crate::server::AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(ref store) = state.gateway.credential_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no credential store" })),
        )
            .into_response();
    };

    let key = body
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let value = body
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "key is required" })),
        )
            .into_response();
    }

    // Validate key format: letters, digits, underscores.
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "key must contain only letters, digits, and underscores" })),
        )
            .into_response();
    }

    match store.set_env_var(key, &value).await {
        Ok(_) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Delete an environment variable by id.
pub async fn env_delete(
    State(state): State<crate::server::AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let Some(ref store) = state.gateway.credential_store else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no credential store" })),
        )
            .into_response();
    };
    match store.delete_env_var(id).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
