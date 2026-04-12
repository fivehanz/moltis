use std::{net::SocketAddr, sync::Arc};

use secrecy::ExposeSecret;

use {
    axum::{
        Json,
        extract::{ConnectInfo, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{delete, get, post},
    },
    axum_extra::extract::Host,
};

use moltis_gateway::{
    auth::CredentialStore, auth_webauthn::SharedWebAuthnRegistry, state::GatewayState,
};

use crate::{
    auth_middleware::{AuthResult, AuthSession, SESSION_COOKIE, check_auth},
    login_guard::LoginGuard,
    server::is_local_connection,
};

/// Auth-related application state.
#[derive(Clone)]
pub struct AuthState {
    pub credential_store: Arc<CredentialStore>,
    pub webauthn_registry: Option<SharedWebAuthnRegistry>,
    pub gateway_state: Arc<GatewayState>,
    pub login_guard: LoginGuard,
}

impl axum::extract::FromRef<AuthState> for Arc<CredentialStore> {
    fn from_ref(state: &AuthState) -> Self {
        Arc::clone(&state.credential_store)
    }
}

impl axum::extract::FromRef<AuthState> for Arc<GatewayState> {
    fn from_ref(state: &AuthState) -> Self {
        Arc::clone(&state.gateway_state)
    }
}

/// Build the auth router with all `/api/auth/*` routes.
pub fn auth_router() -> axum::Router<AuthState> {
    axum::Router::new()
        .route("/status", get(status_handler))
        .route("/setup", post(setup_handler))
        .route("/login", post(login_handler))
        .route("/logout", post(logout_handler))
        .route("/password/change", post(change_password_handler))
        .route(
            "/api-keys",
            get(list_api_keys_handler).post(create_api_key_handler),
        )
        .route("/api-keys/{id}", delete(revoke_api_key_handler))
        .route("/passkeys", get(list_passkeys_handler))
        .route(
            "/passkeys/{id}",
            delete(remove_passkey_handler).patch(rename_passkey_handler),
        )
        .route(
            "/passkey/register/begin",
            post(passkey_register_begin_handler),
        )
        .route(
            "/passkey/register/finish",
            post(passkey_register_finish_handler),
        )
        .route("/passkey/auth/begin", post(passkey_auth_begin_handler))
        .route("/passkey/auth/finish", post(passkey_auth_finish_handler))
        .route(
            "/setup/passkey/register/begin",
            post(setup_passkey_register_begin_handler),
        )
        .route(
            "/setup/passkey/register/finish",
            post(setup_passkey_register_finish_handler),
        )
        .route("/reset", post(reset_auth_handler))
        // Vault endpoints (encryption-at-rest).
        .merge(vault_routes())
}

/// Build vault-specific routes (no-op when vault feature is disabled).
#[cfg(feature = "vault")]
fn vault_routes() -> axum::Router<AuthState> {
    axum::Router::new()
        .route("/vault/status", get(vault_status_handler))
        .route("/vault/unlock", post(vault_unlock_handler))
        .route("/vault/recovery", post(vault_recovery_handler))
}

#[cfg(not(feature = "vault"))]
fn vault_routes() -> axum::Router<AuthState> {
    axum::Router::new()
}

// ── Brute-force block response ────────────────────────────────────────────────

fn blocked_response(reason: crate::login_guard::BlockReason) -> axum::response::Response {
    use crate::login_guard::BlockReason;
    let (message, retry_after) = match reason {
        BlockReason::IpBanned { retry_after } => (
            "too many failed attempts from this address — try again later",
            retry_after,
        ),
        BlockReason::AccountLocked { retry_after } => (
            "account temporarily locked due to suspicious activity — try again later",
            retry_after,
        ),
    };
    let retry_secs = retry_after.as_secs().max(1);
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({
            "code": "LOGIN_BLOCKED",
            "error": message,
            "retry_after_seconds": retry_secs,
        })),
    )
        .into_response();
    if let Ok(val) = retry_secs.to_string().parse() {
        resp.headers_mut()
            .insert(axum::http::header::RETRY_AFTER, val);
    }
    resp
}

// ── Status ───────────────────────────────────────────────────────────────────

async fn status_handler(
    State(state): State<AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let auth_disabled = state.credential_store.is_auth_disabled();
    let localhost_only = state.gateway_state.localhost_only;
    let has_password = state.credential_store.has_password().await.unwrap_or(false);
    let has_passkeys = state.credential_store.has_passkeys().await.unwrap_or(false);

    let is_local = is_local_connection(&headers, addr, state.gateway_state.behind_proxy);
    let auth_result = check_auth(&state.credential_store, &headers, is_local).await;
    let authenticated = matches!(auth_result, AuthResult::Allowed(_));
    let setup_required = matches!(auth_result, AuthResult::SetupRequired);

    let setup_code_required = state.gateway_state.inner.read().await.setup_code.is_some();

    let webauthn_available = state.webauthn_registry.is_some();

    let passkey_origins: Vec<String> = if let Some(registry) = state.webauthn_registry.as_ref() {
        registry.read().await.get_all_origins()
    } else {
        Vec::new()
    };

    if !has_passkeys {
        state
            .gateway_state
            .clear_all_passkey_host_update_pending()
            .await;
    }
    let passkey_host_update_hosts = if has_passkeys {
        state.gateway_state.passkey_host_update_pending().await
    } else {
        Vec::new()
    };
    let passkey_host_update_required = !passkey_host_update_hosts.is_empty();

    let setup_complete = state.credential_store.is_setup_complete();

    Json(serde_json::json!({
        "setup_required": setup_required,
        "setup_complete": setup_complete,
        "has_passkeys": has_passkeys,
        "authenticated": authenticated,
        "auth_disabled": auth_disabled,
        "setup_code_required": setup_code_required,
        "has_password": has_password,
        "localhost_only": localhost_only,
        "webauthn_available": webauthn_available,
        "passkey_origins": passkey_origins,
        "passkey_host_update_required": passkey_host_update_required,
        "passkey_host_update_hosts": passkey_host_update_hosts,
    }))
}

// ── Setup (first run) ────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct SetupRequest {
    password: Option<String>,
    setup_code: Option<String>,
}

async fn setup_handler(
    State(state): State<AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetupRequest>,
) -> impl IntoResponse {
    if state.credential_store.is_setup_complete() {
        return (StatusCode::FORBIDDEN, "setup already completed").into_response();
    }

    let client_ip = crate::request_throttle::resolve_client_ip(
        &headers,
        addr,
        state.gateway_state.behind_proxy,
    );
    const SETUP_ACCOUNT: &str = "__setup__";

    if let Some(block) = state.login_guard.check(client_ip, SETUP_ACCOUNT) {
        return blocked_response(block);
    }

    // Validate setup code if one was generated at startup.
    {
        let inner = state.gateway_state.inner.read().await;
        if let Some(ref expected) = inner.setup_code {
            // Expire setup code after 30 minutes.
            if let Some(created_at) = inner.setup_code_created_at
                && created_at.elapsed() > std::time::Duration::from_secs(30 * 60)
            {
                return (
                    StatusCode::GONE,
                    "setup code has expired — restart the server to generate a new one",
                )
                    .into_response();
            }
            if body.setup_code.as_deref() != Some(expected.expose_secret().as_str()) {
                state.login_guard.record_failure(client_ip, SETUP_ACCOUNT);
                return (StatusCode::FORBIDDEN, "invalid or missing setup code").into_response();
            }
        }
    }

    let password = body.password.unwrap_or_default();

    let is_local = is_local_connection(&headers, addr, state.gateway_state.behind_proxy);
    if password.is_empty() && is_local {
        // Local connection with no password: skip setup without setting one.
        if let Err(e) = state.credential_store.clear_auth_disabled().await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to clear auth-disabled state: {e}"),
            )
                .into_response();
        }
    } else {
        if password.len() < 12 {
            return (
                StatusCode::BAD_REQUEST,
                "password must be at least 12 characters",
            )
                .into_response();
        }
        if let Err(e) = state.credential_store.set_initial_password(&password).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to set password: {e}"),
            )
                .into_response();
        }
    }

    // Initialize the vault when a password was set.
    #[cfg(feature = "vault")]
    let vault_recovery_key = if !password.is_empty() {
        if let Some(ref vault) = state.gateway_state.vault {
            match vault.initialize(&password).await {
                Ok(rk) => {
                    tracing::info!("vault initialized");
                    run_vault_env_migration(&state).await;
                    start_stored_channels_on_vault_unseal(&state).await;
                    Some(rk.phrase().to_owned())
                },
                Err(moltis_vault::VaultError::AlreadyInitialized) => {
                    tracing::debug!("vault already initialized, skipping");
                    None
                },
                Err(e) => {
                    tracing::warn!(error = %e, "vault initialization failed");
                    None
                },
            }
        } else {
            None
        }
    } else {
        None
    };

    // Disconnect pre-setup WebSocket clients and clear setup code.
    state
        .gateway_state
        .disconnect_all_clients("setup_complete")
        .await;
    state.gateway_state.inner.write().await.setup_code = None;
    match state.credential_store.create_session().await {
        Ok(token) => {
            let bp = state.gateway_state.behind_proxy;
            let secure = state.gateway_state.is_secure();
            #[cfg(feature = "vault")]
            if let Some(rk) = vault_recovery_key {
                let domain_attr = localhost_cookie_domain(&headers, bp);
                let secure_attr = if secure {
                    "; Secure"
                } else {
                    ""
                };
                let cookie = format!(
                    "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{domain_attr}{secure_attr}"
                );
                return (
                    StatusCode::OK,
                    [(axum::http::header::SET_COOKIE, cookie)],
                    Json(serde_json::json!({ "ok": true, "recovery_key": rk })),
                )
                    .into_response();
            }
            session_response(token, &headers, bp, secure)
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create session: {e}"),
        )
            .into_response(),
    }
}

// ── Login ────────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct LoginRequest {
    password: String,
}

async fn login_handler(
    State(state): State<AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    let client_ip = crate::request_throttle::resolve_client_ip(
        &headers,
        addr,
        state.gateway_state.behind_proxy,
    );
    const PASSWORD_ACCOUNT: &str = "__password__";

    if let Some(block) = state.login_guard.check(client_ip, PASSWORD_ACCOUNT) {
        return blocked_response(block);
    }

    let ip_str = client_ip.to_string();
    match state.credential_store.verify_password(&body.password).await {
        Ok(true) => {
            state.login_guard.record_success(client_ip);
            state
                .credential_store
                .audit_log("login_success", Some(&ip_str), None)
                .await;
            // Best-effort vault unseal on successful login.
            #[cfg(feature = "vault")]
            if let Some(ref vault) = state.gateway_state.vault {
                match vault.unseal(&body.password).await {
                    Ok(()) => {
                        tracing::info!("vault unsealed on login");
                        run_vault_env_migration(&state).await;
                        start_stored_channels_on_vault_unseal(&state).await;
                    },
                    Err(e) => {
                        tracing::debug!(error = %e, "vault unseal on login skipped");
                    },
                }
            }
            match state.credential_store.create_session().await {
                Ok(token) => {
                    let bp = state.gateway_state.behind_proxy;
                    session_response(token, &headers, bp, state.gateway_state.is_secure())
                },
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("session error: {e}"),
                )
                    .into_response(),
            }
        },
        Ok(false) => {
            state
                .login_guard
                .record_failure(client_ip, PASSWORD_ACCOUNT);
            state
                .credential_store
                .audit_log("login_failure", Some(&ip_str), None)
                .await;
            (StatusCode::UNAUTHORIZED, "invalid password").into_response()
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {e}"),
        )
            .into_response(),
    }
}

// ── Logout ───────────────────────────────────────────────────────────────────

async fn logout_handler(
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        let _ = state.credential_store.delete_session(token).await;
    }
    let bp = state.gateway_state.behind_proxy;
    clear_session_response(&headers, bp, state.gateway_state.is_secure())
}

// ── Reset all auth (requires session) ─────────────────────────────────────────

async fn reset_auth_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    match state.credential_store.reset_all().await {
        Ok(()) => {
            // Disconnect all clients before generating new setup code.
            state
                .gateway_state
                .disconnect_all_clients("auth_reset")
                .await;
            let code = moltis_gateway::auth::generate_setup_code();
            tracing::info!("setup code: {code} (enter this in the browser to set your password)");
            {
                let mut inner = state.gateway_state.inner.write().await;
                inner.setup_code = Some(secrecy::Secret::new(code));
                inner.setup_code_created_at = Some(std::time::Instant::now());
            }
            let bp = state.gateway_state.behind_proxy;
            clear_session_response(&headers, bp, state.gateway_state.tls_active || bp)
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Password change (requires session) ───────────────────────────────────────

#[derive(serde::Deserialize)]
struct ChangePasswordRequest {
    current_password: Option<String>,
    new_password: String,
}

async fn change_password_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ChangePasswordRequest>,
) -> impl IntoResponse {
    if body.new_password.len() < 12 {
        return (
            StatusCode::BAD_REQUEST,
            "new password must be at least 12 characters",
        )
            .into_response();
    }

    let has_password = state.credential_store.has_password().await.unwrap_or(false);

    if !has_password {
        // No password set yet — add one (works even after passkey-only setup).
        return match state
            .credential_store
            .add_password(&body.new_password)
            .await
        {
            Ok(()) => {
                // Initialize the vault now that we have a password.
                #[cfg(feature = "vault")]
                let vault_recovery_key = if let Some(ref vault) = state.gateway_state.vault {
                    match vault.initialize(&body.new_password).await {
                        Ok(rk) => {
                            tracing::info!("vault initialized on first password set");
                            run_vault_env_migration(&state).await;
                            start_stored_channels_on_vault_unseal(&state).await;
                            Some(rk.phrase().to_owned())
                        },
                        Err(moltis_vault::VaultError::AlreadyInitialized) => {
                            tracing::debug!("vault already initialized, unsealing");
                            let _ = vault.unseal(&body.new_password).await;
                            None
                        },
                        Err(e) => {
                            tracing::warn!(error = %e, "vault initialization failed");
                            None
                        },
                    }
                } else {
                    None
                };
                state
                    .gateway_state
                    .disconnect_all_clients("password_changed")
                    .await;
                #[cfg(feature = "vault")]
                if let Some(rk) = vault_recovery_key {
                    return Json(serde_json::json!({ "ok": true, "recovery_key": rk }))
                        .into_response();
                }
                Json(serde_json::json!({ "ok": true })).into_response()
            },
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    let client_ip = crate::request_throttle::resolve_client_ip(
        &headers,
        addr,
        state.gateway_state.behind_proxy,
    );
    const PASSWORD_CHANGE_ACCOUNT: &str = "__password_change__";

    if let Some(block) = state.login_guard.check(client_ip, PASSWORD_CHANGE_ACCOUNT) {
        return blocked_response(block);
    }

    let current_password = body.current_password.unwrap_or_default();
    match state
        .credential_store
        .change_password(&current_password, &body.new_password)
        .await
    {
        Ok(()) => {
            state.login_guard.record_success(client_ip);
            // Best-effort vault password rotation.
            #[cfg(feature = "vault")]
            if let Some(ref vault) = state.gateway_state.vault {
                match vault
                    .change_password(&current_password, &body.new_password)
                    .await
                {
                    Ok(()) => tracing::info!("vault password rotated"),
                    Err(e) => tracing::warn!(error = %e, "vault password rotation failed"),
                }
            }
            state
                .gateway_state
                .disconnect_all_clients("password_changed")
                .await;
            Json(serde_json::json!({ "ok": true })).into_response()
        },
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("incorrect") {
                state
                    .login_guard
                    .record_failure(client_ip, PASSWORD_CHANGE_ACCOUNT);
                (StatusCode::FORBIDDEN, msg).into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, msg).into_response()
            }
        },
    }
}

// ── API Keys (require session) ───────────────────────────────────────────────

async fn list_api_keys_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
) -> impl IntoResponse {
    match state.credential_store.list_api_keys().await {
        Ok(keys) => Json(serde_json::json!({ "api_keys": keys })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct CreateApiKeyRequest {
    label: String,
    /// Optional scopes. If omitted or empty, the key has full access.
    scopes: Option<Vec<String>>,
}

async fn create_api_key_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    Json(body): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    if body.label.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "label is required").into_response();
    }

    // Validate scopes if provided
    if let Some(ref scopes) = body.scopes {
        for scope in scopes {
            if !moltis_gateway::auth::VALID_SCOPES.contains(&scope.as_str()) {
                return (StatusCode::BAD_REQUEST, format!("invalid scope: {scope}"))
                    .into_response();
            }
        }
    }

    match state
        .credential_store
        .create_api_key(body.label.trim(), body.scopes.as_deref())
        .await
    {
        Ok((id, key)) => {
            state
                .credential_store
                .audit_log(
                    "key_created",
                    None,
                    Some(&format!("id={id}, label={}", body.label.trim())),
                )
                .await;
            Json(serde_json::json!({ "id": id, "key": key })).into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn revoke_api_key_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> impl IntoResponse {
    match state.credential_store.revoke_api_key(id).await {
        Ok(()) => {
            state
                .credential_store
                .audit_log("key_revoked", None, Some(&format!("id={id}")))
                .await;
            Json(serde_json::json!({ "ok": true })).into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Passkeys (require session) ───────────────────────────────────────────────

async fn list_passkeys_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
) -> impl IntoResponse {
    match state.credential_store.list_passkeys().await {
        Ok(passkeys) => Json(serde_json::json!({ "passkeys": passkeys })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn remove_passkey_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> impl IntoResponse {
    let was_setup_complete = state.credential_store.is_setup_complete();
    match state.credential_store.remove_passkey(id).await {
        Ok(()) => {
            // If removing the last credential flipped setup_complete from true→false,
            // disconnect all clients so they are forced through re-setup.
            if was_setup_complete && !state.credential_store.is_setup_complete() {
                state
                    .gateway_state
                    .disconnect_all_clients("last_credential_removed")
                    .await;
            }
            Json(serde_json::json!({ "ok": true })).into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct RenamePasskeyRequest {
    name: String,
}

async fn rename_passkey_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(body): Json<RenamePasskeyRequest>,
) -> impl IntoResponse {
    let name = body.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "name cannot be empty").into_response();
    }
    match state.credential_store.rename_passkey(id, name).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a session cookie string, adding `Domain=localhost` when the request
/// arrived on a `.localhost` subdomain (e.g. `moltis.localhost`) so the cookie
/// is shared across all loopback names per RFC 6761.
fn session_response(
    token: String,
    headers: &axum::http::HeaderMap,
    behind_proxy: bool,
    secure: bool,
) -> axum::response::Response {
    let domain_attr = localhost_cookie_domain(headers, behind_proxy);
    let secure_attr = if secure {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{domain_attr}{secure_attr}"
    );
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

fn clear_session_response(
    headers: &axum::http::HeaderMap,
    behind_proxy: bool,
    secure: bool,
) -> axum::response::Response {
    let domain_attr = localhost_cookie_domain(headers, behind_proxy);
    let secure_attr = if secure {
        "; Secure"
    } else {
        ""
    };
    let cookie = format!(
        "{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{domain_attr}{secure_attr}"
    );
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// Return `; Domain=localhost` when the request's `Host` header is a
/// `.localhost` subdomain (e.g. `moltis.localhost:8080`), otherwise `""`.
///
/// Without this, a session cookie set on `localhost` isn't sent by the browser
/// to `moltis.localhost` and vice versa because `Set-Cookie` without a `Domain`
/// attribute is a host-only cookie.  Adding `Domain=localhost` makes the
/// cookie available to `localhost` **and** all its subdomains (RFC 6265 §5.2.3).
fn localhost_cookie_domain(headers: &axum::http::HeaderMap, behind_proxy: bool) -> &'static str {
    let Some(host) = cookie_host(headers, behind_proxy) else {
        return "";
    };

    // Strip port.
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);

    // Behind a proxy we only add Domain=localhost for explicit .localhost
    // forwarded hosts. This avoids setting an invalid Domain when proxies
    // rewrite Host to the upstream loopback address.
    if name.ends_with(".localhost") || (!behind_proxy && name == "localhost") {
        "; Domain=localhost"
    } else {
        ""
    }
}

fn cookie_host(headers: &axum::http::HeaderMap, behind_proxy: bool) -> Option<&str> {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    if !behind_proxy {
        return host;
    }

    headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| raw.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or(host)
}
// ── Passkey registration (requires session) ──────────────────────────────────

async fn passkey_register_begin_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    Host(host): Host,
) -> impl IntoResponse {
    let Some(ref registry) = state.webauthn_registry else {
        return (StatusCode::NOT_IMPLEMENTED, "passkeys not configured").into_response();
    };
    let Some(wa) = host_to_webauthn(&host, registry).await else {
        return (
            StatusCode::BAD_REQUEST,
            "no passkey config for this hostname",
        )
            .into_response();
    };

    let existing = moltis_gateway::auth_webauthn::load_passkeys(&state.credential_store)
        .await
        .unwrap_or_default();

    match wa.start_registration(&existing) {
        Ok((challenge_id, ccr)) => Json(serde_json::json!({
            "challenge_id": challenge_id,
            "options": ccr,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PasskeyRegisterFinishRequest {
    challenge_id: String,
    name: String,
    credential: webauthn_rs::prelude::RegisterPublicKeyCredential,
}

async fn passkey_register_finish_handler(
    _session: AuthSession,
    State(state): State<AuthState>,
    Host(host): Host,
    Json(body): Json<PasskeyRegisterFinishRequest>,
) -> impl IntoResponse {
    let Some(ref registry) = state.webauthn_registry else {
        return (StatusCode::NOT_IMPLEMENTED, "passkeys not configured").into_response();
    };
    let Some(wa) = host_to_webauthn(&host, registry).await else {
        return (
            StatusCode::BAD_REQUEST,
            "no passkey config for this hostname",
        )
            .into_response();
    };

    let passkey = match wa.finish_registration(&body.challenge_id, &body.credential) {
        Ok(pk) => pk,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let cred_id = passkey.cred_id().as_ref();
    let data = match serde_json::to_vec(&passkey) {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let name = if body.name.trim().is_empty() {
        "Passkey"
    } else {
        body.name.trim()
    };

    match state
        .credential_store
        .store_passkey(cred_id, name, &data)
        .await
    {
        Ok(id) => {
            state
                .gateway_state
                .clear_passkey_host_update_pending(&host)
                .await;
            Json(serde_json::json!({ "id": id })).into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Passkey authentication (no session required) ─────────────────────────────

async fn passkey_auth_begin_handler(
    State(state): State<AuthState>,
    Host(host): Host,
) -> impl IntoResponse {
    let Some(ref registry) = state.webauthn_registry else {
        return (StatusCode::NOT_IMPLEMENTED, "passkeys not configured").into_response();
    };
    let Some(wa) = host_to_webauthn(&host, registry).await else {
        return (
            StatusCode::BAD_REQUEST,
            "no passkey config for this hostname",
        )
            .into_response();
    };

    let passkeys = match moltis_gateway::auth_webauthn::load_passkeys(&state.credential_store).await
    {
        Ok(pks) => pks,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    match wa.start_authentication(&passkeys) {
        Ok((challenge_id, rcr)) => Json(serde_json::json!({
            "challenge_id": challenge_id,
            "options": rcr,
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PasskeyAuthFinishRequest {
    challenge_id: String,
    credential: webauthn_rs::prelude::PublicKeyCredential,
}

async fn passkey_auth_finish_handler(
    State(state): State<AuthState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Host(host): Host,
    headers: axum::http::HeaderMap,
    Json(body): Json<PasskeyAuthFinishRequest>,
) -> impl IntoResponse {
    let client_ip = crate::request_throttle::resolve_client_ip(
        &headers,
        addr,
        state.gateway_state.behind_proxy,
    );
    const PASSKEY_ACCOUNT: &str = "__passkey__";

    if let Some(block) = state.login_guard.check(client_ip, PASSKEY_ACCOUNT) {
        return blocked_response(block);
    }

    let Some(ref registry) = state.webauthn_registry else {
        return (StatusCode::NOT_IMPLEMENTED, "passkeys not configured").into_response();
    };
    let Some(wa) = host_to_webauthn(&host, registry).await else {
        return (
            StatusCode::BAD_REQUEST,
            "no passkey config for this hostname",
        )
            .into_response();
    };

    match wa.finish_authentication(&body.challenge_id, &body.credential) {
        Ok(_result) => {
            state.login_guard.record_success(client_ip);
            match state.credential_store.create_session().await {
                Ok(token) => {
                    let bp = state.gateway_state.behind_proxy;
                    session_response(token, &headers, bp, state.gateway_state.tls_active || bp)
                },
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
            }
        },
        Err(e) => {
            state.login_guard.record_failure(client_ip, PASSKEY_ACCOUNT);
            (StatusCode::UNAUTHORIZED, e.to_string()).into_response()
        },
    }
}

// ── Setup-time passkey registration (setup code instead of session) ───────────

#[derive(serde::Deserialize)]
struct SetupPasskeyBeginRequest {
    setup_code: Option<String>,
}

async fn setup_passkey_register_begin_handler(
    State(state): State<AuthState>,
    Host(host): Host,
    Json(body): Json<SetupPasskeyBeginRequest>,
) -> impl IntoResponse {
    if state.credential_store.is_setup_complete() {
        return (StatusCode::FORBIDDEN, "setup already completed").into_response();
    }

    // Validate setup code if one was generated at startup.
    {
        let inner = state.gateway_state.inner.read().await;
        if let Some(ref expected) = inner.setup_code
            && body.setup_code.as_deref() != Some(expected.expose_secret().as_str())
        {
            return (StatusCode::FORBIDDEN, "invalid or missing setup code").into_response();
        }
    }

    let Some(ref registry) = state.webauthn_registry else {
        return (StatusCode::NOT_IMPLEMENTED, "passkeys not configured").into_response();
    };
    let Some(wa) = host_to_webauthn(&host, registry).await else {
        return (
            StatusCode::BAD_REQUEST,
            "no passkey config for this hostname",
        )
            .into_response();
    };

    let existing = moltis_gateway::auth_webauthn::load_passkeys(&state.credential_store)
        .await
        .unwrap_or_default();

    match wa.start_registration(&existing) {
        Ok((challenge_id, ccr)) => Json(serde_json::json!({
            "challenge_id": challenge_id,
            "options": ccr,
        }))
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct SetupPasskeyFinishRequest {
    challenge_id: String,
    name: String,
    setup_code: Option<String>,
    credential: webauthn_rs::prelude::RegisterPublicKeyCredential,
}

async fn setup_passkey_register_finish_handler(
    State(state): State<AuthState>,
    Host(host): Host,
    headers: axum::http::HeaderMap,
    Json(body): Json<SetupPasskeyFinishRequest>,
) -> impl IntoResponse {
    if state.credential_store.is_setup_complete() {
        return (StatusCode::FORBIDDEN, "setup already completed").into_response();
    }

    // Validate setup code if one was generated at startup.
    {
        let inner = state.gateway_state.inner.read().await;
        if let Some(ref expected) = inner.setup_code
            && body.setup_code.as_deref() != Some(expected.expose_secret().as_str())
        {
            return (StatusCode::FORBIDDEN, "invalid or missing setup code").into_response();
        }
    }

    let Some(ref registry) = state.webauthn_registry else {
        return (StatusCode::NOT_IMPLEMENTED, "passkeys not configured").into_response();
    };
    let Some(wa) = host_to_webauthn(&host, registry).await else {
        return (
            StatusCode::BAD_REQUEST,
            "no passkey config for this hostname",
        )
            .into_response();
    };

    let passkey = match wa.finish_registration(&body.challenge_id, &body.credential) {
        Ok(pk) => pk,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };

    let cred_id = passkey.cred_id().as_ref();
    let data = match serde_json::to_vec(&passkey) {
        Ok(d) => d,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let name = if body.name.trim().is_empty() {
        "Passkey"
    } else {
        body.name.trim()
    };

    if let Err(e) = state
        .credential_store
        .store_passkey(cred_id, name, &data)
        .await
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }

    state
        .gateway_state
        .clear_passkey_host_update_pending(&host)
        .await;

    if let Err(e) = state.credential_store.mark_setup_complete().await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to mark setup complete: {e}"),
        )
            .into_response();
    }

    // Disconnect pre-setup WebSocket clients and clear setup code.
    state
        .gateway_state
        .disconnect_all_clients("setup_complete")
        .await;
    state.gateway_state.inner.write().await.setup_code = None;
    match state.credential_store.create_session().await {
        Ok(token) => {
            let bp = state.gateway_state.behind_proxy;
            session_response(token, &headers, bp, state.gateway_state.tls_active || bp)
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to create session: {e}"),
        )
            .into_response(),
    }
}

/// Look up the `WebAuthnState` matching the request hostname (`Host` or
/// `:authority`). Matching includes RP IDs and explicitly allowed origin hosts.
async fn host_to_webauthn(
    host: &str,
    registry: &SharedWebAuthnRegistry,
) -> Option<Arc<moltis_gateway::auth_webauthn::WebAuthnState>> {
    registry.read().await.get_for_host(host)
}

fn extract_session_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    let cookie_header = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())?;
    crate::auth_middleware::parse_cookie(cookie_header, SESSION_COOKIE)
}

// ── Vault handlers ──────────────────────────────────────────────────────────

#[cfg(feature = "vault")]
async fn vault_status_handler(State(state): State<AuthState>) -> impl IntoResponse {
    let status = if let Some(ref vault) = state.gateway_state.vault {
        match vault.status().await {
            Ok(s) => format!("{s:?}").to_lowercase(),
            Err(_) => "error".to_owned(),
        }
    } else {
        "disabled".to_owned()
    };
    Json(serde_json::json!({ "status": status }))
}

#[cfg(feature = "vault")]
#[derive(serde::Deserialize)]
struct VaultUnlockRequest {
    password: String,
}

#[cfg(feature = "vault")]
async fn vault_unlock_handler(
    State(state): State<AuthState>,
    Json(body): Json<VaultUnlockRequest>,
) -> impl IntoResponse {
    let Some(ref vault) = state.gateway_state.vault else {
        return (StatusCode::NOT_FOUND, "vault not available").into_response();
    };
    match vault.unseal(&body.password).await {
        Ok(()) => {
            run_vault_env_migration(&state).await;
            start_stored_channels_on_vault_unseal(&state).await;
            Json(serde_json::json!({ "ok": true })).into_response()
        },
        Err(moltis_vault::VaultError::BadCredential) => {
            (StatusCode::LOCKED, "invalid password").into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(feature = "vault")]
#[derive(serde::Deserialize)]
struct VaultRecoveryRequest {
    recovery_key: String,
}

#[cfg(feature = "vault")]
async fn vault_recovery_handler(
    State(state): State<AuthState>,
    Json(body): Json<VaultRecoveryRequest>,
) -> impl IntoResponse {
    let Some(ref vault) = state.gateway_state.vault else {
        return (StatusCode::NOT_FOUND, "vault not available").into_response();
    };
    match vault.unseal_with_recovery(&body.recovery_key).await {
        Ok(()) => {
            run_vault_env_migration(&state).await;
            start_stored_channels_on_vault_unseal(&state).await;
            Json(serde_json::json!({ "ok": true })).into_response()
        },
        Err(moltis_vault::VaultError::BadCredential) => {
            (StatusCode::LOCKED, "invalid recovery key").into_response()
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Migrate plaintext secrets to encrypted storage after vault unseal.
#[cfg(feature = "vault")]
async fn run_vault_env_migration(state: &AuthState) {
    if let Some(vault) = state.credential_store.vault() {
        let pool = state.credential_store.db_pool();
        match moltis_vault::migration::migrate_env_vars(vault, pool).await {
            Ok(n) if n > 0 => {
                tracing::info!(count = n, "migrated env vars to encrypted");
            },
            Ok(_) => {},
            Err(e) => {
                tracing::warn!(error = %e, "env var migration failed");
            },
        }
        match moltis_vault::migration::migrate_ssh_keys(vault, pool).await {
            Ok(n) if n > 0 => {
                tracing::info!(count = n, "migrated ssh keys to encrypted");
            },
            Ok(_) => {},
            Err(e) => {
                tracing::warn!(error = %e, "ssh key migration failed");
            },
        }
    }
}

/// Start stored channel accounts after vault unseal.
///
/// When the vault is unsealed, previously encrypted channel configs become
/// decryptable. This function reads all stored channels and starts any that
/// aren't already running in the channel registry. This is the counterpart to
/// the startup-time channel loading in `server.rs` — it handles the case where
/// the vault was sealed at startup and channels couldn't be started then.
#[cfg(feature = "vault")]
#[tracing::instrument(skip(state))]
async fn start_stored_channels_on_vault_unseal(state: &AuthState) {
    let Some(registry) = state.gateway_state.services.channel_registry.as_ref() else {
        tracing::debug!("no channel registry available, skipping channel startup on vault unseal");
        return;
    };
    let Some(store) = state.gateway_state.services.channel_store.as_ref() else {
        tracing::debug!("no channel store available, skipping channel startup on vault unseal");
        return;
    };

    let stored = match store.list().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to list stored channels on vault unseal");
            return;
        },
    };

    if stored.is_empty() {
        return;
    }

    for ch in stored {
        // Skip channel types with no registered plugin.
        if registry.get(&ch.channel_type).is_none() {
            tracing::debug!(
                account_id = ch.account_id,
                channel_type = ch.channel_type,
                "unsupported channel type on vault unseal, skipping stored account"
            );
            continue;
        }

        // Skip accounts that are already running.
        if registry.resolve_channel_type(&ch.account_id).is_some() {
            continue;
        }

        tracing::info!(
            account_id = ch.account_id,
            channel_type = ch.channel_type,
            "starting stored channel on vault unseal"
        );

        if let Err(e) = registry
            .start_account(&ch.channel_type, &ch.account_id, ch.config)
            .await
        {
            tracing::warn!(
                account_id = ch.account_id,
                channel_type = ch.channel_type,
                error = %e,
                "failed to start stored channel on vault unseal"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn headers_with_host(host: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::HOST,
            host.parse().expect("valid host header"),
        );
        h
    }

    #[test]
    fn localhost_cookie_domain_plain_localhost() {
        let h = headers_with_host("localhost:8080");
        assert_eq!(localhost_cookie_domain(&h, false), "; Domain=localhost");
    }

    #[test]
    fn localhost_cookie_domain_moltis_subdomain() {
        let h = headers_with_host("moltis.localhost:59263");
        assert_eq!(localhost_cookie_domain(&h, false), "; Domain=localhost");
    }

    #[test]
    fn localhost_cookie_domain_bare_localhost_no_port() {
        let h = headers_with_host("localhost");
        assert_eq!(localhost_cookie_domain(&h, false), "; Domain=localhost");
    }

    #[test]
    fn localhost_cookie_domain_external_host_omits_domain() {
        let h = headers_with_host("example.com:443");
        assert_eq!(localhost_cookie_domain(&h, false), "");
    }

    #[test]
    fn localhost_cookie_domain_tailscale_host_omits_domain() {
        let h = headers_with_host("mybox.tail12345.ts.net:8080");
        assert_eq!(localhost_cookie_domain(&h, false), "");
    }

    #[test]
    fn localhost_cookie_domain_ip_address_omits_domain() {
        let h = headers_with_host("192.168.1.100:8080");
        assert_eq!(localhost_cookie_domain(&h, false), "");
    }

    #[test]
    fn localhost_cookie_domain_no_host_header_omits_domain() {
        let h = axum::http::HeaderMap::new();
        assert_eq!(localhost_cookie_domain(&h, false), "");
    }

    #[test]
    fn localhost_cookie_domain_proxy_mode_ignores_upstream_localhost_host() {
        let h = headers_with_host("localhost:13131");
        assert_eq!(localhost_cookie_domain(&h, true), "");
    }

    #[test]
    fn localhost_cookie_domain_proxy_mode_uses_forwarded_host() {
        let mut h = headers_with_host("localhost:13131");
        h.insert("x-forwarded-host", "chat.example.com".parse().unwrap());
        assert_eq!(localhost_cookie_domain(&h, true), "");
    }

    #[test]
    fn localhost_cookie_domain_proxy_mode_supports_forwarded_localhost_subdomain() {
        let mut h = headers_with_host("localhost:13131");
        h.insert("x-forwarded-host", "moltis.localhost:8080".parse().unwrap());
        assert_eq!(localhost_cookie_domain(&h, true), "; Domain=localhost");
    }

    #[test]
    fn session_response_includes_domain_for_localhost() {
        let h = headers_with_host("moltis.localhost:8080");
        let resp = session_response("test-token".into(), &h, false, false);
        let cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("login response must set a session cookie")
            .to_str()
            .expect("cookie header must be valid UTF-8");
        assert!(
            cookie.contains("; Domain=localhost"),
            "cookie should include Domain=localhost for .localhost host, got: {cookie}"
        );
        assert!(cookie.contains("moltis_session=test-token"));
        assert!(
            !cookie.contains("; Secure"),
            "cookie should NOT include Secure when not using TLS, got: {cookie}"
        );
    }

    #[test]
    fn session_response_omits_domain_for_external_host() {
        let h = headers_with_host("example.com:443");
        let resp = session_response("test-token".into(), &h, false, false);
        let cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("login response must set a session cookie")
            .to_str()
            .expect("cookie header must be valid UTF-8");
        assert!(
            !cookie.contains("Domain="),
            "cookie should NOT include Domain for external host, got: {cookie}"
        );
    }

    #[test]
    fn session_response_includes_secure_when_tls_active() {
        let h = headers_with_host("localhost:8443");
        let resp = session_response("test-token".into(), &h, false, true);
        let cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("login response must set a session cookie")
            .to_str()
            .expect("cookie header must be valid UTF-8");
        assert!(
            cookie.contains("; Secure"),
            "cookie should include Secure when TLS is active, got: {cookie}"
        );
    }

    #[test]
    fn clear_session_response_includes_secure_when_tls_active() {
        let h = headers_with_host("localhost:8443");
        let resp = clear_session_response(&h, false, true);
        let cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("clear response must set a session cookie")
            .to_str()
            .expect("cookie header must be valid UTF-8");
        assert!(
            cookie.contains("; Secure"),
            "clear cookie should include Secure when TLS is active, got: {cookie}"
        );
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn clear_session_response_includes_domain_for_localhost() {
        let h = headers_with_host("localhost:18080");
        let resp = clear_session_response(&h, false, false);
        let cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("clear response must set a session cookie")
            .to_str()
            .expect("cookie header must be valid UTF-8");
        assert!(
            cookie.contains("; Domain=localhost"),
            "clear cookie should include Domain=localhost, got: {cookie}"
        );
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn session_response_proxy_mode_omits_localhost_domain_without_forwarded_host() {
        let h = headers_with_host("localhost:13131");
        let resp = session_response("test-token".into(), &h, true, true);
        let cookie = resp
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .expect("login response must set a session cookie")
            .to_str()
            .expect("cookie header must be valid UTF-8");
        assert!(
            !cookie.contains("Domain="),
            "cookie should omit Domain in proxy mode when only upstream localhost host is visible, got: {cookie}"
        );
        assert!(
            cookie.contains("; Secure"),
            "cookie should include Secure in proxy mode (proxy implies TLS), got: {cookie}"
        );
    }
}

#[cfg(test)]
#[cfg(feature = "vault")]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod vault_unseal_tests {
    use {
        super::*,
        async_trait::async_trait,
        moltis_channels::{
            ChannelRegistry,
            config_view::ChannelConfigView,
            plugin::{ChannelOutbound, ChannelPlugin, ChannelStreamOutbound, StreamEvent},
            store::{ChannelStore, StoredChannel},
        },
    };

    use {
        moltis_channels::plugin::ChannelStatus, moltis_common::types::ReplyPayload,
        std::sync::Mutex, tokio::sync::RwLock,
    };

    /// Helper to build a minimal AuthState with the given services.
    async fn build_auth_state(services: moltis_gateway::services::GatewayServices) -> AuthState {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let auth_config = moltis_config::AuthConfig::default();
        let cred_store = Arc::new(
            CredentialStore::with_config(pool, &auth_config)
                .await
                .unwrap(),
        );
        let gateway_state =
            GatewayState::new(moltis_gateway::auth::resolve_auth(None, None), services);
        AuthState {
            credential_store: cred_store,
            webauthn_registry: None,
            gateway_state,
            login_guard: LoginGuard::new(),
        }
    }

    // ── Mock implementations ─────────────────────────────────────────────

    struct MockChannelStore {
        channels: Vec<StoredChannel>,
        should_fail: bool,
    }

    impl MockChannelStore {
        fn new(channels: Vec<StoredChannel>) -> Self {
            Self {
                channels,
                should_fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                channels: vec![],
                should_fail: true,
            }
        }
    }

    #[async_trait]
    impl ChannelStore for MockChannelStore {
        async fn list(&self) -> moltis_channels::Result<Vec<StoredChannel>> {
            if self.should_fail {
                return Err(moltis_channels::Error::unavailable("store error"));
            }
            Ok(self.channels.clone())
        }

        async fn get(
            &self,
            _channel_type: &str,
            _account_id: &str,
        ) -> moltis_channels::Result<Option<StoredChannel>> {
            Ok(None)
        }

        async fn upsert(&self, _channel: StoredChannel) -> moltis_channels::Result<()> {
            Ok(())
        }

        async fn delete(
            &self,
            _channel_type: &str,
            _account_id: &str,
        ) -> moltis_channels::Result<()> {
            Ok(())
        }
    }

    /// Null outbound that does nothing.
    struct NullOutbound;

    #[async_trait]
    impl ChannelOutbound for NullOutbound {
        async fn send_text(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> moltis_channels::Result<()> {
            Ok(())
        }

        async fn send_media(
            &self,
            _: &str,
            _: &str,
            _: &ReplyPayload,
            _: Option<&str>,
        ) -> moltis_channels::Result<()> {
            Ok(())
        }
    }

    /// Null stream outbound.
    struct NullStreamOutbound;

    #[async_trait]
    impl ChannelStreamOutbound for NullStreamOutbound {
        async fn send_stream(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            mut stream: moltis_channels::plugin::StreamReceiver,
        ) -> moltis_channels::Result<()> {
            while let Some(event) = stream.recv().await {
                if matches!(event, StreamEvent::Done | StreamEvent::Error(_)) {
                    break;
                }
            }
            Ok(())
        }
    }

    /// Test plugin that records start_account calls.
    struct RecordingPlugin {
        id: String,
        started_accounts: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        should_fail_start: bool,
    }

    impl RecordingPlugin {
        fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                started_accounts: Arc::new(Mutex::new(Vec::new())),
                should_fail_start: false,
            }
        }

        fn failing(id: &str) -> Self {
            Self {
                id: id.to_string(),
                started_accounts: Arc::new(Mutex::new(Vec::new())),
                should_fail_start: true,
            }
        }
    }

    #[async_trait]
    impl ChannelPlugin for RecordingPlugin {
        fn id(&self) -> &str {
            &self.id
        }

        fn name(&self) -> &str {
            &self.id
        }

        async fn start_account(
            &mut self,
            account_id: &str,
            config: serde_json::Value,
        ) -> moltis_channels::Result<()> {
            if self.should_fail_start {
                return Err(moltis_channels::Error::unavailable("start failed"));
            }
            self.started_accounts
                .lock()
                .unwrap()
                .push((account_id.to_string(), config));
            Ok(())
        }

        async fn stop_account(&mut self, _account_id: &str) -> moltis_channels::Result<()> {
            Ok(())
        }

        fn outbound(&self) -> Option<&dyn ChannelOutbound> {
            None
        }

        fn status(&self) -> Option<&dyn ChannelStatus> {
            None
        }

        fn has_account(&self, _account_id: &str) -> bool {
            false
        }

        fn account_ids(&self) -> Vec<String> {
            Vec::new()
        }

        fn account_config(&self, _account_id: &str) -> Option<Box<dyn ChannelConfigView>> {
            None
        }

        fn update_account_config(
            &self,
            _account_id: &str,
            _config: serde_json::Value,
        ) -> moltis_channels::Result<()> {
            Ok(())
        }

        fn shared_outbound(&self) -> Arc<dyn ChannelOutbound> {
            Arc::new(NullOutbound)
        }

        fn shared_stream_outbound(&self) -> Arc<dyn ChannelStreamOutbound> {
            Arc::new(NullStreamOutbound)
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn returns_early_when_channel_registry_is_none() {
        let services = moltis_gateway::services::GatewayServices::noop();
        let state = build_auth_state(services).await;
        // noop() has no channel_registry — should return without panicking
        start_stored_channels_on_vault_unseal(&state).await;
    }

    #[tokio::test]
    async fn returns_early_when_channel_store_is_none() {
        let registry = Arc::new(ChannelRegistry::new());
        let services =
            moltis_gateway::services::GatewayServices::noop().with_channel_registry(registry);
        // No channel_store set — should return without panicking
        let state = build_auth_state(services).await;
        start_stored_channels_on_vault_unseal(&state).await;
    }

    #[tokio::test]
    async fn logs_warning_when_store_list_fails() {
        let registry = Arc::new(ChannelRegistry::new());
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::failing());
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;
        // Should log a warning but not panic
        start_stored_channels_on_vault_unseal(&state).await;
    }

    #[tokio::test]
    async fn returns_when_store_is_empty() {
        let registry = Arc::new(ChannelRegistry::new());
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::new(vec![]));
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;
        start_stored_channels_on_vault_unseal(&state).await;
    }

    #[tokio::test]
    async fn skips_channels_with_unsupported_type() {
        let registry = Arc::new(ChannelRegistry::new());
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::new(vec![StoredChannel {
            account_id: "acct1".to_string(),
            channel_type: "unknown_type".to_string(),
            config: serde_json::json!({}),
            created_at: 0,
            updated_at: 0,
        }]));
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;
        // Should not panic — logs a warning about unsupported type
        start_stored_channels_on_vault_unseal(&state).await;
    }

    #[tokio::test]
    async fn skips_channels_already_running() {
        let started = Arc::new(Mutex::new(Vec::new()));
        let plugin = RecordingPlugin::new("telegram");
        let started_clone = Arc::clone(&started);
        let plugin_with_tracking = RecordingPluginWithTracking {
            inner: plugin,
            started_accounts: started_clone,
        };

        let mut registry = ChannelRegistry::new();
        registry
            .register(Arc::new(RwLock::new(plugin_with_tracking)))
            .await;

        // Pre-start an account so it's already running
        registry
            .start_account("telegram", "acct1", serde_json::json!({"token": "x"}))
            .await
            .unwrap();

        let registry = Arc::new(registry);
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::new(vec![StoredChannel {
            account_id: "acct1".to_string(),
            channel_type: "telegram".to_string(),
            config: serde_json::json!({"token": "new"}),
            created_at: 0,
            updated_at: 0,
        }]));
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;

        start_stored_channels_on_vault_unseal(&state).await;

        // The account was already running, so start_account should NOT have been
        // called again (resolve_channel_type returned Some, so it was skipped).
        // Only the pre-start call should be recorded.
        let calls = started.lock().unwrap();
        assert_eq!(
            calls.len(),
            1,
            "should not re-start already running account"
        );
        assert_eq!(calls[0].0, "acct1");
    }

    #[tokio::test]
    async fn starts_channels_successfully() {
        let plugin = RecordingPlugin::new("telegram");
        let started_accounts = Arc::clone(&plugin.started_accounts);

        let mut registry = ChannelRegistry::new();
        registry.register(Arc::new(RwLock::new(plugin))).await;

        let registry = Arc::new(registry);
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::new(vec![
            StoredChannel {
                account_id: "bot1".to_string(),
                channel_type: "telegram".to_string(),
                config: serde_json::json!({"token": "abc123"}),
                created_at: 1000,
                updated_at: 2000,
            },
            StoredChannel {
                account_id: "bot2".to_string(),
                channel_type: "telegram".to_string(),
                config: serde_json::json!({"token": "def456"}),
                created_at: 1001,
                updated_at: 2001,
            },
        ]));
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;

        start_stored_channels_on_vault_unseal(&state).await;

        let calls = started_accounts.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "bot1");
        assert_eq!(calls[1].0, "bot2");
        assert_eq!(calls[0].1["token"], "abc123");
        assert_eq!(calls[1].1["token"], "def456");
    }

    #[tokio::test]
    async fn logs_warning_and_continues_when_start_account_fails() {
        let plugin = RecordingPlugin::failing("telegram");
        let started_accounts = Arc::clone(&plugin.started_accounts);

        let mut registry = ChannelRegistry::new();
        registry.register(Arc::new(RwLock::new(plugin))).await;

        let registry = Arc::new(registry);
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::new(vec![StoredChannel {
            account_id: "bot1".to_string(),
            channel_type: "telegram".to_string(),
            config: serde_json::json!({"token": "bad"}),
            created_at: 0,
            updated_at: 0,
        }]));
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;

        // Should not panic — logs a warning about failed start
        start_stored_channels_on_vault_unseal(&state).await;

        // The plugin should have attempted the start (and recorded it before failing)
        let calls = started_accounts.lock().unwrap();
        assert_eq!(
            calls.len(),
            0,
            "failing plugin should not record successful starts"
        );
    }

    #[tokio::test]
    async fn mixed_channels_skips_unsupported_and_starts_supported() {
        let plugin = RecordingPlugin::new("telegram");
        let started_accounts = Arc::clone(&plugin.started_accounts);

        let mut registry = ChannelRegistry::new();
        registry.register(Arc::new(RwLock::new(plugin))).await;

        let registry = Arc::new(registry);
        let store: Arc<dyn ChannelStore> = Arc::new(MockChannelStore::new(vec![
            StoredChannel {
                account_id: "unsupported_acct".to_string(),
                channel_type: "unsupported_type".to_string(),
                config: serde_json::json!({}),
                created_at: 0,
                updated_at: 0,
            },
            StoredChannel {
                account_id: "tg_bot".to_string(),
                channel_type: "telegram".to_string(),
                config: serde_json::json!({"token": "xyz"}),
                created_at: 0,
                updated_at: 0,
            },
        ]));
        let services = moltis_gateway::services::GatewayServices::noop()
            .with_channel_registry(registry)
            .with_channel_store(store);
        let state = build_auth_state(services).await;

        start_stored_channels_on_vault_unseal(&state).await;

        let calls = started_accounts.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "tg_bot");
    }

    // ── Helper wrapper for the "already running" test ───────────────────

    /// Wrapper that tracks start_account calls through the registry path
    /// (which calls the plugin directly).
    struct RecordingPluginWithTracking {
        inner: RecordingPlugin,
        started_accounts: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
    }

    #[async_trait]
    impl ChannelPlugin for RecordingPluginWithTracking {
        fn id(&self) -> &str {
            self.inner.id()
        }

        fn name(&self) -> &str {
            self.inner.name()
        }

        async fn start_account(
            &mut self,
            account_id: &str,
            config: serde_json::Value,
        ) -> moltis_channels::Result<()> {
            self.started_accounts
                .lock()
                .unwrap()
                .push((account_id.to_string(), config.clone()));
            self.inner.start_account(account_id, config).await
        }

        async fn stop_account(&mut self, account_id: &str) -> moltis_channels::Result<()> {
            self.inner.stop_account(account_id).await
        }

        fn outbound(&self) -> Option<&dyn ChannelOutbound> {
            self.inner.outbound()
        }

        fn status(&self) -> Option<&dyn ChannelStatus> {
            self.inner.status()
        }

        fn has_account(&self, account_id: &str) -> bool {
            self.inner.has_account(account_id)
        }

        fn account_ids(&self) -> Vec<String> {
            self.inner.account_ids()
        }

        fn account_config(&self, account_id: &str) -> Option<Box<dyn ChannelConfigView>> {
            self.inner.account_config(account_id)
        }

        fn update_account_config(
            &self,
            account_id: &str,
            config: serde_json::Value,
        ) -> moltis_channels::Result<()> {
            self.inner.update_account_config(account_id, config)
        }

        fn shared_outbound(&self) -> Arc<dyn ChannelOutbound> {
            self.inner.shared_outbound()
        }

        fn shared_stream_outbound(&self) -> Arc<dyn ChannelStreamOutbound> {
            self.inner.shared_stream_outbound()
        }
    }
}
