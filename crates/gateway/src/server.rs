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

#[cfg(feature = "web-ui")]
use axum::http::StatusCode;

use {moltis_channels::ChannelPlugin, moltis_protocol::TICK_INTERVAL_MS};

use moltis_agents::providers::ProviderRegistry;

use moltis_tools::approval::ApprovalManager;

use {
    moltis_projects::ProjectStore,
    moltis_sessions::{
        metadata::{SessionMetadata, SqliteSessionMetadata},
        store::SessionStore,
    },
};

use crate::{
    approval::{GatewayApprovalBroadcaster, LiveExecApprovalService},
    auth,
    broadcast::broadcast_tick,
    chat::{LiveChatService, LiveModelService},
    methods::MethodRegistry,
    provider_setup::LiveProviderSetupService,
    services::GatewayServices,
    session::LiveSessionService,
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
        .route("/assets/style.css", get(css_handler))
        .route("/assets/app.js", get(js_handler))
        .fallback(spa_fallback);

    router.layer(cors).with_state(app_state)
}

/// Start the gateway HTTP + WebSocket server.
pub async fn start_gateway(
    bind: &str,
    port: u16,
    log_buffer: Option<crate::logs::LogBuffer>,
) -> anyhow::Result<()> {
    // Resolve auth from environment (MOLTIS_TOKEN / MOLTIS_PASSWORD).
    let token = std::env::var("MOLTIS_TOKEN").ok();
    let password = std::env::var("MOLTIS_PASSWORD").ok();
    let resolved_auth = auth::resolve_auth(token, password);

    // Load config file (moltis.toml / .yaml / .json) if present.
    let config = moltis_config::discover_and_load();

    // Merge any previously saved API keys into the provider config so they
    // survive gateway restarts without requiring env vars.
    let key_store = crate::provider_setup::KeyStore::new();
    let effective_providers =
        crate::provider_setup::config_with_saved_keys(&config.providers, &key_store);

    // Discover LLM providers from env + config + saved keys.
    let registry = Arc::new(tokio::sync::RwLock::new(
        ProviderRegistry::from_env_with_config(&effective_providers),
    ));
    let provider_summary = registry.read().await.provider_summary();

    // Create shared approval manager.
    let approval_manager = Arc::new(ApprovalManager::default());

    let mut services = GatewayServices::noop();

    // Wire live logs service if a log buffer is available.
    if let Some(ref buf) = log_buffer {
        services.logs = Arc::new(crate::logs::LiveLogsService::new(buf.clone()));
    }

    services.exec_approval = Arc::new(LiveExecApprovalService::new(Arc::clone(&approval_manager)));
    services.provider_setup = Arc::new(LiveProviderSetupService::new(
        Arc::clone(&registry),
        config.providers.clone(),
    ));
    if !registry.read().await.is_empty() {
        services = services.with_model(Arc::new(LiveModelService::new(Arc::clone(&registry))));
    }

    // Initialize data directory and SQLite database.
    let data_dir = directories::ProjectDirs::from("", "", "moltis")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".moltis"));
    std::fs::create_dir_all(&data_dir).ok();

    // Enable log persistence so entries survive restarts.
    if let Some(ref buf) = log_buffer {
        buf.enable_persistence(data_dir.join("logs.jsonl"));
    }
    let db_path = data_dir.join("moltis.db");
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let db_pool = sqlx::SqlitePool::connect(&db_url)
        .await
        .expect("failed to open moltis.db");

    // Create tables.
    moltis_projects::SqliteProjectStore::init(&db_pool)
        .await
        .expect("failed to init projects table");
    SqliteSessionMetadata::init(&db_pool)
        .await
        .expect("failed to init sessions table");

    crate::message_log_store::SqliteMessageLog::init(&db_pool)
        .await
        .expect("failed to init message_log table");
    let message_log: Arc<dyn moltis_channels::message_log::MessageLog> =
        Arc::new(crate::message_log_store::SqliteMessageLog::new(db_pool.clone()));

    // Migrate from projects.toml if it exists.
    let config_dir = directories::ProjectDirs::from("", "", "moltis")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".moltis"));
    let projects_toml_path = config_dir.join("projects.toml");
    if projects_toml_path.exists() {
        info!("migrating projects.toml to SQLite");
        let old_store = moltis_projects::TomlProjectStore::new(projects_toml_path.clone());
        let sqlite_store = moltis_projects::SqliteProjectStore::new(db_pool.clone());
        if let Ok(projects) =
            <moltis_projects::TomlProjectStore as moltis_projects::ProjectStore>::list(&old_store)
                .await
        {
            for p in projects {
                if let Err(e) = sqlite_store.upsert(p).await {
                    tracing::warn!("failed to migrate project: {e}");
                }
            }
        }
        let bak = projects_toml_path.with_extension("toml.bak");
        std::fs::rename(&projects_toml_path, &bak).ok();
    }

    // Migrate from metadata.json if it exists.
    let sessions_dir = data_dir.join("sessions");
    let metadata_json_path = sessions_dir.join("metadata.json");
    if metadata_json_path.exists() {
        info!("migrating metadata.json to SQLite");
        if let Ok(old_meta) = SessionMetadata::load(metadata_json_path.clone()) {
            let sqlite_meta = SqliteSessionMetadata::new(db_pool.clone());
            for entry in old_meta.list() {
                if let Err(e) = sqlite_meta.upsert(&entry.key, entry.label.clone()).await {
                    tracing::warn!("failed to migrate session {}: {e}", entry.key);
                }
                if entry.model.is_some() {
                    sqlite_meta.set_model(&entry.key, entry.model.clone()).await;
                }
                sqlite_meta.touch(&entry.key, entry.message_count).await;
                if entry.project_id.is_some() {
                    sqlite_meta
                        .set_project_id(&entry.key, entry.project_id.clone())
                        .await;
                }
            }
        }
        let bak = metadata_json_path.with_extension("json.bak");
        std::fs::rename(&metadata_json_path, &bak).ok();
    }

    // Wire stores.
    let project_store: Arc<dyn moltis_projects::ProjectStore> =
        Arc::new(moltis_projects::SqliteProjectStore::new(db_pool.clone()));
    let session_store = Arc::new(SessionStore::new(sessions_dir));
    let session_metadata = Arc::new(SqliteSessionMetadata::new(db_pool.clone()));

    // Session service wired below after sandbox_router is created.

    // Wire live project service.
    services.project = Arc::new(crate::project::LiveProjectService::new(Arc::clone(
        &project_store,
    )));

    // Initialize cron service with file-backed store.
    let cron_store: Arc<dyn moltis_cron::store::CronStore> =
        match moltis_cron::store_file::FileStore::default_path() {
            Ok(fs) => Arc::new(fs),
            Err(e) => {
                tracing::warn!("cron file store unavailable ({e}), using in-memory");
                Arc::new(moltis_cron::store_memory::InMemoryStore::new())
            },
        };

    // Deferred reference: populated once GatewayState is ready.
    let deferred_state: Arc<tokio::sync::OnceCell<Arc<GatewayState>>> =
        Arc::new(tokio::sync::OnceCell::new());

    // System event: inject text into the main session and trigger an agent response.
    let sys_state = Arc::clone(&deferred_state);
    let on_system_event: moltis_cron::service::SystemEventFn = Arc::new(move |text| {
        let st = Arc::clone(&sys_state);
        tokio::spawn(async move {
            if let Some(state) = st.get() {
                let chat = state.chat().await;
                let params = serde_json::json!({ "text": text });
                if let Err(e) = chat.send(params).await {
                    tracing::error!("cron system event failed: {e}");
                }
            }
        });
    });

    // Agent turn: run an isolated LLM turn (no session history) and return the output.
    let agent_state = Arc::clone(&deferred_state);
    let on_agent_turn: moltis_cron::service::AgentTurnFn = Arc::new(move |req| {
        let st = Arc::clone(&agent_state);
        Box::pin(async move {
            let state = st
                .get()
                .ok_or_else(|| anyhow::anyhow!("gateway not ready"))?;
            let chat = state.chat().await;
            // Send into an isolated session keyed by a unique id so it doesn't
            // pollute the main conversation.
            let session_key = format!("cron:{}", uuid::Uuid::new_v4());
            let params = serde_json::json!({
                "text": req.message,
                "_session_key": session_key,
            });
            chat.send(params).await.map_err(|e| anyhow::anyhow!(e))?;
            Ok("agent turn dispatched".into())
        })
    });

    let cron_service =
        moltis_cron::service::CronService::new(cron_store, on_system_event, on_agent_turn);

    // Wire cron into gateway services.
    let live_cron = Arc::new(crate::cron::LiveCronService::new(Arc::clone(&cron_service)));
    services = services.with_cron(live_cron);

    // Build sandbox router from config (shared across sessions).
    let sandbox_config = moltis_tools::sandbox::SandboxConfig {
        mode: match config.tools.exec.sandbox.mode.as_str() {
            "all" => moltis_tools::sandbox::SandboxMode::All,
            "non-main" | "nonmain" => moltis_tools::sandbox::SandboxMode::NonMain,
            _ => moltis_tools::sandbox::SandboxMode::Off,
        },
        scope: match config.tools.exec.sandbox.scope.as_str() {
            "agent" => moltis_tools::sandbox::SandboxScope::Agent,
            "shared" => moltis_tools::sandbox::SandboxScope::Shared,
            _ => moltis_tools::sandbox::SandboxScope::Session,
        },
        workspace_mount: match config.tools.exec.sandbox.workspace_mount.as_str() {
            "rw" => moltis_tools::sandbox::WorkspaceMount::Rw,
            "none" => moltis_tools::sandbox::WorkspaceMount::None,
            _ => moltis_tools::sandbox::WorkspaceMount::Ro,
        },
        image: config.tools.exec.sandbox.image.clone(),
        container_prefix: config.tools.exec.sandbox.container_prefix.clone(),
        no_network: config.tools.exec.sandbox.no_network,
        resource_limits: moltis_tools::sandbox::ResourceLimits {
            memory_limit: config
                .tools
                .exec
                .sandbox
                .resource_limits
                .memory_limit
                .clone(),
            cpu_quota: config.tools.exec.sandbox.resource_limits.cpu_quota,
            pids_max: config.tools.exec.sandbox.resource_limits.pids_max,
        },
    };
    let sandbox_router = Arc::new(moltis_tools::sandbox::SandboxRouter::new(sandbox_config));

    // Load any persisted sandbox overrides from session metadata.
    {
        for entry in session_metadata.list().await {
            if let Some(enabled) = entry.sandbox_enabled {
                sandbox_router.set_override(&entry.key, enabled).await;
            }
        }
    }

    // Wire live session service with sandbox router and project store.
    services.session = Arc::new(
        LiveSessionService::new(Arc::clone(&session_store), Arc::clone(&session_metadata))
            .with_sandbox_router(Arc::clone(&sandbox_router))
            .with_project_store(Arc::clone(&project_store)),
    );

    // Wire channel store and Telegram channel service.
    {
        use moltis_channels::store::ChannelStore;

        crate::channel_store::SqliteChannelStore::init(&db_pool)
            .await
            .expect("failed to init channels table");
        let channel_store: Arc<dyn ChannelStore> =
            Arc::new(crate::channel_store::SqliteChannelStore::new(db_pool.clone()));

        let channel_sink = Arc::new(crate::channel_events::GatewayChannelEventSink::new(
            Arc::clone(&deferred_state),
        ));
        let mut tg_plugin = moltis_telegram::TelegramPlugin::new()
            .with_message_log(Arc::clone(&message_log))
            .with_event_sink(channel_sink);

        // Start channels from config file (these take precedence).
        let tg_accounts = &config.channels.telegram;
        let mut started: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (account_id, account_config) in tg_accounts {
            if let Err(e) = tg_plugin
                .start_account(account_id, account_config.clone())
                .await
            {
                tracing::warn!(account_id, "failed to start telegram account: {e}");
            } else {
                started.insert(account_id.clone());
            }
        }

        // Load persisted channels that weren't in the config file.
        match channel_store.list().await {
            Ok(stored) => {
                info!("{} stored channel(s) found in database", stored.len());
                for ch in stored {
                    if started.contains(&ch.account_id) {
                        info!(account_id = ch.account_id, "skipping stored channel (already started from config)");
                        continue;
                    }
                    info!(account_id = ch.account_id, channel_type = ch.channel_type, "starting stored channel");
                    if let Err(e) = tg_plugin
                        .start_account(&ch.account_id, ch.config)
                        .await
                    {
                        tracing::warn!(
                            account_id = ch.account_id,
                            "failed to start stored telegram account: {e}"
                        );
                    } else {
                        started.insert(ch.account_id);
                    }
                }
            },
            Err(e) => {
                tracing::warn!("failed to load stored channels: {e}");
            },
        }

        if !started.is_empty() {
            info!("{} telegram account(s) started", started.len());
        }

        // Grab shared outbound before moving tg_plugin into the channel service.
        let tg_outbound = tg_plugin.shared_outbound();
        services = services.with_channel_outbound(tg_outbound);

        services.channel = Arc::new(crate::channel::LiveChannelService::new(
            tg_plugin,
            channel_store,
            Arc::clone(&message_log),
        ));
    }

    let state = GatewayState::with_sandbox_router(
        resolved_auth,
        services,
        Arc::clone(&approval_manager),
        Some(Arc::clone(&sandbox_router)),
    );
    // Populate the deferred reference so cron callbacks can reach the gateway.
    let _ = deferred_state.set(Arc::clone(&state));

    // Wire live chat service (needs state reference, so done after state creation).
    if !registry.read().await.is_empty() {
        let broadcaster = Arc::new(GatewayApprovalBroadcaster::new(Arc::clone(&state)));
        let exec_tool = moltis_tools::exec::ExecTool::default()
            .with_approval(Arc::clone(&approval_manager), broadcaster)
            .with_sandbox_router(Arc::clone(&sandbox_router));

        let cron_tool = moltis_tools::cron_tool::CronTool::new(Arc::clone(&cron_service));

        let mut tool_registry = moltis_agents::tool_registry::ToolRegistry::new();
        tool_registry.register(Box::new(exec_tool));
        tool_registry.register(Box::new(cron_tool));
        let live_chat = Arc::new(
            LiveChatService::new(
                Arc::clone(&registry),
                Arc::clone(&state),
                Arc::clone(&session_store),
                Arc::clone(&session_metadata),
            )
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

    // Spawn log broadcast task: forwards captured tracing events to WS clients.
    if let Some(buf) = log_buffer {
        let log_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut rx = buf.subscribe();
            loop {
                match rx.recv().await {
                    Ok(entry) => {
                        if let Ok(payload) = serde_json::to_value(&entry) {
                            crate::broadcast::broadcast(
                                &log_state,
                                "logs.entry",
                                payload,
                                crate::broadcast::BroadcastOpts {
                                    drop_if_slow: true,
                                    ..Default::default()
                                },
                            )
                            .await;
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }

    // Start the cron scheduler (loads persisted jobs, arms the timer).
    if let Err(e) = cron_service.start().await {
        tracing::warn!("failed to start cron scheduler: {e}");
    }

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

/// SPA fallback: serve `index.html` for any path not matched by an explicit
/// route (assets, ws, health). This lets client-side routing handle `/crons`,
/// `/methods`, etc.
#[cfg(feature = "web-ui")]
async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    // Reject requests that look like missing asset files so the browser gets
    // a proper 404 instead of HTML.
    let path = uri.path();
    if path.starts_with("/assets/") || path.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    Html(include_str!("assets/index.html")).into_response()
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
