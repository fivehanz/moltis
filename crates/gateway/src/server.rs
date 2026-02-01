use std::{net::SocketAddr, sync::Arc};

#[cfg(feature = "tls")]
use std::path::PathBuf;

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
use axum::{extract::Path, http::StatusCode};

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

#[cfg(feature = "tls")]
use crate::tls::CertManager;

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
        .route("/assets/v/{version}/{*path}", get(versioned_asset_handler))
        .route("/assets/{*path}", get(asset_handler))
        .route("/api/bootstrap", get(api_bootstrap_handler))
        .route("/api/skills", get(api_skills_handler))
        .route("/api/skills/search", get(api_skills_search_handler))
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
    let message_log: Arc<dyn moltis_channels::message_log::MessageLog> = Arc::new(
        crate::message_log_store::SqliteMessageLog::new(db_pool.clone()),
    );

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
        let channel_store: Arc<dyn ChannelStore> = Arc::new(
            crate::channel_store::SqliteChannelStore::new(db_pool.clone()),
        );

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
                        info!(
                            account_id = ch.account_id,
                            "skipping stored channel (already started from config)"
                        );
                        continue;
                    }
                    info!(
                        account_id = ch.account_id,
                        channel_type = ch.channel_type,
                        "starting stored channel"
                    );
                    if let Err(e) = tg_plugin.start_account(&ch.account_id, ch.config).await {
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
            Arc::clone(&session_metadata),
        ));
    }

    services = services.with_session_metadata(Arc::clone(&session_metadata));
    services = services.with_session_store(Arc::clone(&session_store));

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

    #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
    let mut app = build_gateway_app(Arc::clone(&state), Arc::clone(&methods));

    let addr: SocketAddr = format!("{bind}:{port}").parse()?;

    // Resolve TLS configuration (only when compiled with the `tls` feature).
    #[cfg(feature = "tls")]
    let tls_active = config.tls.enabled;
    #[cfg(not(feature = "tls"))]
    let tls_active = false;

    #[cfg(feature = "tls")]
    let mut ca_cert_path: Option<PathBuf> = None;
    #[cfg(feature = "tls")]
    let mut rustls_config: Option<rustls::ServerConfig> = None;

    #[cfg(feature = "tls")]
    if tls_active {
        let tls_config = &config.tls;
        let (ca_path, cert_path, key_path) = if let (Some(cert_str), Some(key_str)) =
            (&tls_config.cert_path, &tls_config.key_path)
        {
            // User-provided certs.
            let cert = PathBuf::from(cert_str);
            let key = PathBuf::from(key_str);
            let ca = tls_config.ca_cert_path.as_ref().map(PathBuf::from);
            (ca, cert, key)
        } else if tls_config.auto_generate {
            // Auto-generate certificates.
            let mgr = crate::tls::FsCertManager::new()?;
            let (ca, cert, key) = mgr.ensure_certs()?;
            (Some(ca), cert, key)
        } else {
            anyhow::bail!(
                "TLS is enabled but no certificates configured and auto_generate is false"
            );
        };

        ca_cert_path = ca_path.clone();

        let mgr = crate::tls::FsCertManager::new()?;
        rustls_config = Some(mgr.build_rustls_config(&cert_path, &key_path)?);

        // Add /certs/ca.pem route to the main HTTPS app if we have a CA cert.
        if let Some(ref ca) = ca_path {
            let ca_bytes = Arc::new(std::fs::read(ca)?);
            let ca_clone = Arc::clone(&ca_bytes);
            app = app.route(
                "/certs/ca.pem",
                get(move || {
                    let data = Arc::clone(&ca_clone);
                    async move {
                        (
                            [
                                ("content-type", "application/x-pem-file"),
                                (
                                    "content-disposition",
                                    "attachment; filename=\"moltis-ca.pem\"",
                                ),
                            ],
                            data.as_ref().clone(),
                        )
                    }
                }),
            );
        }
    }

    // Count enabled skills and repos for startup banner.
    let (skill_count, repo_count) = {
        use moltis_skills::discover::{FsSkillDiscoverer, SkillDiscoverer};
        let cwd = std::env::current_dir().unwrap_or_default();
        let discoverer = FsSkillDiscoverer::new(FsSkillDiscoverer::default_paths(&cwd));
        let sc = discoverer.discover().await.map(|s| s.len()).unwrap_or(0);
        let rc = moltis_skills::manifest::ManifestStore::default_path()
            .ok()
            .map(|p| {
                let store = moltis_skills::manifest::ManifestStore::new(p);
                store.load().map(|m| m.repos.len()).unwrap_or(0)
            })
            .unwrap_or(0);
        (sc, rc)
    };

    // Startup banner.
    let scheme = if tls_active {
        "https"
    } else {
        "http"
    };
    #[cfg_attr(not(feature = "tls"), allow(unused_mut))]
    let mut lines = vec![
        format!("moltis gateway v{}", state.version),
        format!(
            "protocol v{}, listening on {}://{}",
            moltis_protocol::PROTOCOL_VERSION,
            scheme,
            addr,
        ),
        format!("{} methods registered", methods.method_names().len()),
        format!("llm: {}", provider_summary),
        format!(
            "skills: {} enabled, {} repo{}",
            skill_count,
            repo_count,
            if repo_count == 1 {
                ""
            } else {
                "s"
            }
        ),
    ];
    #[cfg(feature = "tls")]
    if tls_active {
        if let Some(ref ca) = ca_cert_path {
            let http_port = config.tls.http_redirect_port.unwrap_or(18790);
            lines.push(format!(
                "CA cert: http://{}:{}/certs/ca.pem",
                bind, http_port
            ));
            lines.push(format!("  or: {}", ca.display()));
        }
        lines.push("run `moltis trust-ca` to remove browser warnings".into());
    }
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

    #[cfg(feature = "tls")]
    if tls_active {
        // Spawn HTTP redirect server on secondary port.
        if let Some(ref ca) = ca_cert_path {
            let http_port = config.tls.http_redirect_port.unwrap_or(18790);
            let bind_clone = bind.to_string();
            let ca_clone = ca.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    crate::tls::start_http_redirect_server(&bind_clone, http_port, port, &ca_clone)
                        .await
                {
                    tracing::error!("HTTP redirect server failed: {e}");
                }
            });
        }

        // Run HTTPS server.
        let tls_cfg = rustls_config.expect("rustls config must be set when TLS is active");
        let rustls_cfg = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls_cfg));
        axum_server::bind_rustls(addr, rustls_cfg)
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await?;
        return Ok(());
    }

    // Plain HTTP server (existing behavior, or TLS feature disabled).
    let listener = tokio::net::TcpListener::bind(addr).await?;
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
/// `/logs`, etc.
///
/// Injects a `<script>` tag with pre-fetched bootstrap data (channels,
/// sessions, models, projects) so the UI can render synchronously without
/// waiting for the WebSocket handshake — similar to the gon pattern in Rails.
#[cfg(feature = "web-ui")]
async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path();
    if path.starts_with("/assets/") || path.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let raw = read_asset("index.html")
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();

    let body = if is_dev_assets() {
        // Dev: no versioned URLs, just serve directly with no-cache
        raw.replace("__BUILD_TS__", "dev")
    } else {
        // Production: inject content-hash versioned URLs for immutable caching
        static HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(asset_content_hash);
        let versioned = format!("/assets/v/{}/", *HASH);
        raw.replace("__BUILD_TS__", &*HASH)
            .replace("/assets/", &versioned)
    };

    ([("cache-control", "no-cache, no-store")], Html(body)).into_response()
}

#[cfg(feature = "web-ui")]
async fn api_bootstrap_handler(State(state): State<AppState>) -> impl IntoResponse {
    let gw = &state.gateway;
    let (channels, sessions, models, projects) = tokio::join!(
        gw.services.channel.status(),
        gw.services.session.list(),
        gw.services.model.list(),
        gw.services.project.list(),
    );
    Json(serde_json::json!({
        "channels": channels.ok(),
        "sessions": sessions.ok(),
        "models": models.ok(),
        "projects": projects.ok(),
    }))
}

/// Lightweight skills overview: repo summaries + enabled skills only.
/// Full skill lists are loaded on-demand via /api/skills/search.
/// Merges both skills and plugins manifests for the UI.
#[cfg(feature = "web-ui")]
async fn api_skills_handler(State(state): State<AppState>) -> impl IntoResponse {
    let gw = &state.gateway;

    // Skill repos
    let skill_repos = gw
        .services
        .skills
        .repos_list()
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    // Plugin repos
    let plugin_repos = gw
        .services
        .plugins
        .repos_list()
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut all_repos = skill_repos;
    all_repos.extend(plugin_repos);

    // Enabled skills from skills manifest
    let mut enabled_skills: Vec<serde_json::Value> =
        if let Ok(path) = moltis_skills::manifest::ManifestStore::default_path() {
            let store = moltis_skills::manifest::ManifestStore::new(path);
            store
                .load()
                .map(|m| {
                    m.repos
                        .iter()
                        .flat_map(|repo| {
                            let source = repo.source.clone();
                            repo.skills.iter().filter(|s| s.enabled).map(move |s| {
                                serde_json::json!({
                                    "name": s.name,
                                    "source": source,
                                    "enabled": true,
                                })
                            })
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

    // Enabled skills from plugins manifest
    if let Ok(path) = moltis_plugins::install::default_manifest_path() {
        let store = moltis_skills::manifest::ManifestStore::new(path);
        if let Ok(m) = store.load() {
            for repo in &m.repos {
                let source = repo.source.clone();
                for s in &repo.skills {
                    if s.enabled {
                        enabled_skills.push(serde_json::json!({
                            "name": s.name,
                            "source": source,
                            "enabled": true,
                        }));
                    }
                }
            }
        }
    }

    Json(serde_json::json!({
        "skills": enabled_skills,
        "repos": all_repos,
    }))
}

/// Search skills within a specific repo. Query params: source, q (optional).
/// If q is empty, returns all skills for the repo. Searches both skills and plugins.
#[cfg(feature = "web-ui")]
async fn api_skills_search_handler(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let source = params.get("source").cloned().unwrap_or_default();
    let query = params.get("q").cloned().unwrap_or_default().to_lowercase();

    let gw = &state.gateway;

    // Search skills repos first.
    let skill_repos = gw
        .services
        .skills
        .repos_list_full()
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    // Search plugins repos.
    let plugin_repos = gw
        .services
        .plugins
        .repos_list_full()
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut all_repos = skill_repos;
    all_repos.extend(plugin_repos);

    let skills: Vec<serde_json::Value> = all_repos
        .into_iter()
        .find(|repo| {
            repo.get("source")
                .and_then(|s| s.as_str())
                .map(|s| s == source)
                .unwrap_or(false)
        })
        .and_then(|repo| repo.get("skills").and_then(|s| s.as_array()).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|skill| {
            if query.is_empty() {
                return true;
            }
            let name = skill
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_lowercase();
            let display = skill
                .get("display_name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_lowercase();
            let desc = skill
                .get("description")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_lowercase();
            name.contains(&query) || display.contains(&query) || desc.contains(&query)
        })
        .take(30)
        .collect();

    Json(serde_json::json!({ "skills": skills }))
}

#[cfg(feature = "web-ui")]
static ASSETS: include_dir::Dir = include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/assets");

// ── Asset serving: filesystem (dev) or embedded (release) ───────────────────

/// Filesystem path to serve assets from, if available. Checked once at startup.
/// Set via `MOLTIS_ASSETS_DIR` env var, or auto-detected from the crate source
/// tree when running via `cargo run`.
#[cfg(feature = "web-ui")]
static FS_ASSETS_DIR: std::sync::LazyLock<Option<std::path::PathBuf>> =
    std::sync::LazyLock::new(|| {
        use std::path::PathBuf;

        // Explicit env var takes precedence
        if let Ok(dir) = std::env::var("MOLTIS_ASSETS_DIR") {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                info!("Serving assets from filesystem: {}", p.display());
                return Some(p);
            }
        }

        // Auto-detect: works when running from the repo via `cargo run`
        let cargo_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/assets");
        if cargo_dir.is_dir() {
            info!("Serving assets from filesystem: {}", cargo_dir.display());
            return Some(cargo_dir);
        }

        info!("Serving assets from embedded binary");
        None
    });

/// Whether we're serving from the filesystem (dev mode) or embedded (release).
#[cfg(feature = "web-ui")]
fn is_dev_assets() -> bool {
    FS_ASSETS_DIR.is_some()
}

/// Compute a short content hash of all embedded assets. Only used in release
/// mode (embedded assets) for cache-busting versioned URLs.
#[cfg(feature = "web-ui")]
fn asset_content_hash() -> String {
    use std::{collections::BTreeMap, hash::Hasher};

    let mut files = BTreeMap::new();
    let mut stack: Vec<&include_dir::Dir<'_>> = vec![&ASSETS];
    while let Some(dir) = stack.pop() {
        for file in dir.files() {
            files.insert(file.path().display().to_string(), file.contents());
        }
        for sub in dir.dirs() {
            stack.push(sub);
        }
    }

    let mut h = std::hash::DefaultHasher::new();
    for (path, contents) in &files {
        h.write(path.as_bytes());
        h.write(contents);
    }
    format!("{:016x}", h.finish())
}

#[cfg(feature = "web-ui")]
fn mime_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "mjs" => "application/javascript; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        _ => "application/octet-stream",
    }
}

/// Read an asset file, preferring filesystem over embedded.
#[cfg(feature = "web-ui")]
fn read_asset(path: &str) -> Option<Vec<u8>> {
    if let Some(dir) = FS_ASSETS_DIR.as_ref() {
        let file_path = dir.join(path);
        // Prevent path traversal
        if file_path.starts_with(dir) {
            if let Ok(bytes) = std::fs::read(&file_path) {
                return Some(bytes);
            }
        }
    }
    ASSETS.get_file(path).map(|f| f.contents().to_vec())
}

/// Versioned assets: `/assets/v/<hash>/path` — immutable, cached forever.
#[cfg(feature = "web-ui")]
async fn versioned_asset_handler(
    Path((_version, path)): Path<(String, String)>,
) -> impl IntoResponse {
    let cache = if is_dev_assets() {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    serve_asset(&path, cache)
}

/// Unversioned assets: `/assets/path` — always no-cache.
#[cfg(feature = "web-ui")]
async fn asset_handler(Path(path): Path<String>) -> impl IntoResponse {
    serve_asset(&path, "no-cache")
}

#[cfg(feature = "web-ui")]
fn serve_asset(path: &str, cache_control: &'static str) -> axum::response::Response {
    match read_asset(path) {
        Some(body) => (
            StatusCode::OK,
            [
                ("content-type", mime_for_path(path)),
                ("cache-control", cache_control),
            ],
            body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
