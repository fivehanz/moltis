use std::{net::SocketAddr, sync::Arc};

#[cfg(feature = "web-ui")]
use axum::response::Html;
use {
    axum::{
        Router,
        extract::{ConnectInfo, State, WebSocketUpgrade},
        response::{IntoResponse, Json},
        routing::get,
    },
    tower_http::cors::{Any, CorsLayer},
    tracing::info,
};

use moltis_protocol::TICK_INTERVAL_MS;

use moltis_agents::providers::ProviderRegistry;

use moltis_tools::approval::ApprovalManager;

use crate::{
    approval::{GatewayApprovalBroadcaster, LiveExecApprovalService},
    auth,
    broadcast::broadcast_tick,
    chat::{LiveChatService, LiveModelService},
    methods::MethodRegistry,
    provider_setup::LiveProviderSetupService,
    services::GatewayServices,
    state::GatewayState,
    ws::handle_connection,
};

// ── Shared app state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    gateway: Arc<GatewayState>,
    methods: Arc<MethodRegistry>,
}

// ── Server startup ───────────────────────────────────────────────────────────

/// Build the gateway router (shared between production startup and tests).
pub fn build_gateway_app(state: Arc<GatewayState>, methods: Arc<MethodRegistry>) -> Router {
    let app_state = AppState {
        gateway: state,
        methods,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let router = Router::new()
        .route("/health", get(health_handler))
        .route("/ws", get(ws_upgrade_handler));

    #[cfg(feature = "web-ui")]
    let router = router
        .route("/", get(root_handler))
        .route("/assets/style.css", get(css_handler))
        .route("/assets/app.js", get(js_handler));

    router.layer(cors).with_state(app_state)
}

/// Start the gateway HTTP + WebSocket server.
pub async fn start_gateway(bind: &str, port: u16) -> anyhow::Result<()> {
    // Resolve auth from environment (MOLTIS_TOKEN / MOLTIS_PASSWORD).
    let token = std::env::var("MOLTIS_TOKEN").ok();
    let password = std::env::var("MOLTIS_PASSWORD").ok();
    let resolved_auth = auth::resolve_auth(token, password);

    // Load config file (moltis.toml / .yaml / .json) if present.
    let config = moltis_config::discover_and_load();

    // Discover LLM providers from env + config.
    let registry = Arc::new(tokio::sync::RwLock::new(
        ProviderRegistry::from_env_with_config(&config.providers),
    ));
    let provider_summary = registry.read().await.provider_summary();

    // Create shared approval manager.
    let approval_manager = Arc::new(ApprovalManager::default());

    let mut services = GatewayServices::noop();
    services.exec_approval = Arc::new(LiveExecApprovalService::new(Arc::clone(&approval_manager)));
    services.provider_setup = Arc::new(LiveProviderSetupService::new(
        Arc::clone(&registry),
        config.providers.clone(),
    ));
    if !registry.read().await.is_empty() {
        services = services.with_model(Arc::new(LiveModelService::new(Arc::clone(&registry))));
    }

    let state = GatewayState::new(resolved_auth, services, Arc::clone(&approval_manager));

    // Wire live chat service (needs state reference, so done after state creation).
    if !registry.read().await.is_empty() {
        let broadcaster = Arc::new(GatewayApprovalBroadcaster::new(Arc::clone(&state)));
        let exec_tool = moltis_tools::exec::ExecTool::default()
            .with_approval(Arc::clone(&approval_manager), broadcaster);

        let mut tool_registry = moltis_agents::tool_registry::ToolRegistry::new();
        tool_registry.register(Box::new(exec_tool));
        let live_chat = Arc::new(
            LiveChatService::new(Arc::clone(&registry), Arc::clone(&state))
                .with_tools(tool_registry),
        );
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
    ws.on_upgrade(move |socket| handle_connection(socket, state.gateway, state.methods, addr))
}

#[cfg(feature = "web-ui")]
async fn root_handler() -> impl IntoResponse {
    Html(include_str!("assets/index.html"))
}

#[cfg(feature = "web-ui")]
async fn css_handler() -> impl IntoResponse {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("assets/style.css"),
    )
}

#[cfg(feature = "web-ui")]
async fn js_handler() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        include_str!("assets/app.js"),
    )
}
