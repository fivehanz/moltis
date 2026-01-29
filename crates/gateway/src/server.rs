use std::net::SocketAddr;
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, State, WebSocketUpgrade},
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use moltis_protocol::TICK_INTERVAL_MS;

use moltis_agents::providers::ProviderRegistry;

use crate::auth;
use crate::broadcast::broadcast_tick;
use crate::chat::{LiveChatService, LiveModelService};
use crate::methods::MethodRegistry;
use crate::services::GatewayServices;
use crate::state::GatewayState;
use crate::ws::handle_connection;

// ── Shared app state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    gateway: Arc<GatewayState>,
    methods: Arc<MethodRegistry>,
}

// ── Server startup ───────────────────────────────────────────────────────────

/// Build the gateway router (shared between production startup and tests).
pub fn build_gateway_app(
    state: Arc<GatewayState>,
    methods: Arc<MethodRegistry>,
) -> Router {
    let app_state = AppState {
        gateway: state,
        methods,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/health", get(health_handler))
        .route("/ws", get(ws_upgrade_handler))
        .route("/", get(root_handler))
        .layer(cors)
        .with_state(app_state)
}

/// Start the gateway HTTP + WebSocket server.
pub async fn start_gateway(bind: &str, port: u16) -> anyhow::Result<()> {
    // Resolve auth from environment (MOLTIS_TOKEN / MOLTIS_PASSWORD).
    let token = std::env::var("MOLTIS_TOKEN").ok();
    let password = std::env::var("MOLTIS_PASSWORD").ok();
    let resolved_auth = auth::resolve_auth(token, password);

    // Discover LLM providers from env.
    let registry = Arc::new(ProviderRegistry::from_env());
    let provider_summary = registry.provider_summary();

    let mut services = GatewayServices::noop();
    if !registry.is_empty() {
        services = services.with_model(Arc::new(LiveModelService::new(Arc::clone(&registry))));
    }

    let state = GatewayState::new(resolved_auth, services);

    // Wire live chat service (needs state reference, so done after state creation).
    if !registry.is_empty() {
        let live_chat = Arc::new(LiveChatService::new(
            Arc::clone(&registry),
            Arc::clone(&state),
        ));
        state.set_chat(live_chat).await;
    }

    let methods = Arc::new(MethodRegistry::new());

    let app = build_gateway_app(Arc::clone(&state), Arc::clone(&methods));

    let addr: SocketAddr = format!("{bind}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Startup banner.
    let lines = [
        format!("moltis gateway v{}", state.version),
        format!(
            "protocol v{}, listening on {}",
            moltis_protocol::PROTOCOL_VERSION,
            addr
        ),
        format!("{} methods registered", methods.method_names().len()),
        format!("llm: {}", provider_summary),
    ];
    let width = lines.iter().map(|l| l.len()).max().unwrap_or(0) + 4;
    info!("┌{}┐", "─".repeat(width));
    for line in &lines {
        info!("│  {:<w$}│", line, w = width - 2);
    }
    info!("└{}┘", "─".repeat(width));

    // Spawn tick timer.
    let tick_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(TICK_INTERVAL_MS));
        loop {
            interval.tick().await;
            broadcast_tick(&tick_state).await;
        }
    });

    // Run the server with ConnectInfo for remote IP extraction.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let count = state.gateway.client_count().await;
    Json(serde_json::json!({
        "status": "ok",
        "version": state.gateway.version,
        "protocol": moltis_protocol::PROTOCOL_VERSION,
        "connections": count,
    }))
}

async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| {
        handle_connection(socket, state.gateway, state.methods, addr)
    })
}

async fn root_handler() -> impl IntoResponse {
    Html(include_str!("chat_ui.html"))
}
