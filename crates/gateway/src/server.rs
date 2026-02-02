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

use moltis_tools::{approval::ApprovalManager, image_cache::ImageBuilder};

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
    auth_routes::{AuthState, auth_router},
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
pub struct AppState {
    pub gateway: Arc<GatewayState>,
    pub methods: Arc<MethodRegistry>,
}

// ── Server startup ───────────────────────────────────────────────────────────

/// Build the gateway router (shared between production startup and tests).
pub fn build_gateway_app(state: Arc<GatewayState>, methods: Arc<MethodRegistry>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mut router = Router::new()
        .route("/health", get(health_handler))
        .route("/ws", get(ws_upgrade_handler));

    // Nest auth routes if credential store is available.
    if let Some(ref cred_store) = state.credential_store {
        let auth_state = AuthState {
            credential_store: Arc::clone(cred_store),
            webauthn_state: state.webauthn_state.clone(),
            gateway_state: Arc::clone(&state),
        };
        router = router.nest("/api/auth", auth_router().with_state(auth_state));
    }

    let app_state = AppState {
        gateway: state,
        methods,
    };

    #[cfg(feature = "web-ui")]
    let router = {
        // Protected API routes — require auth when credential store is configured.
        let protected = Router::new()
            .route("/api/bootstrap", get(api_bootstrap_handler))
            .route("/api/gon", get(api_gon_handler))
            .route("/api/skills", get(api_skills_handler))
            .route("/api/skills/search", get(api_skills_search_handler))
            .route(
                "/api/images/cached",
                get(api_cached_images_handler).delete(api_prune_cached_images_handler),
            )
            .route(
                "/api/images/cached/{tag}",
                axum::routing::delete(api_delete_cached_image_handler),
            )
            .route(
                "/api/images/build",
                axum::routing::post(api_build_image_handler),
            )
            .route(
                "/api/images/check-packages",
                axum::routing::post(api_check_packages_handler),
            )
            .route(
                "/api/images/default",
                get(api_get_default_image_handler).put(api_set_default_image_handler),
            )
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                crate::auth_middleware::require_auth,
            ));

        // Public routes (assets, SPA fallback).
        router
            .route("/assets/v/{version}/{*path}", get(versioned_asset_handler))
            .route("/assets/{*path}", get(asset_handler))
            .merge(protected)
            .fallback(spa_fallback)
    };

    router.layer(cors).with_state(app_state)
}

/// Start the gateway HTTP + WebSocket server.
pub async fn start_gateway(
    bind: &str,
    port: u16,
    log_buffer: Option<crate::logs::LogBuffer>,
    config_dir: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    // Apply config directory override before loading config.
    if let Some(dir) = config_dir {
        moltis_config::set_config_dir(dir);
    }

    // Resolve auth from environment (MOLTIS_TOKEN / MOLTIS_PASSWORD).
    let token = std::env::var("MOLTIS_TOKEN").ok();
    let password = std::env::var("MOLTIS_PASSWORD").ok();
    let resolved_auth = auth::resolve_auth(token, password.clone());

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

    // Wire live onboarding service.
    let onboarding_config_path = moltis_config::find_or_default_config_path();
    let live_onboarding =
        moltis_onboarding::service::LiveOnboardingService::new(onboarding_config_path);
    services = services.with_onboarding(Arc::new(
        crate::onboarding::GatewayOnboardingService::new(live_onboarding),
    ));
    services.provider_setup = Arc::new(LiveProviderSetupService::new(
        Arc::clone(&registry),
        config.providers.clone(),
    ));
    if !registry.read().await.is_empty() {
        services = services.with_model(Arc::new(LiveModelService::new(Arc::clone(&registry))));
    }

    // Initialize data directory and SQLite database.
    let data_dir = data_dir.unwrap_or_else(moltis_config::data_dir);
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

    // Initialize credential store (auth tables).
    let credential_store = Arc::new(
        auth::CredentialStore::new(db_pool.clone())
            .await
            .expect("failed to init credential store"),
    );

    // Initialize WebAuthn state for passkey support.
    // RP ID defaults to "localhost"; override with MOLTIS_WEBAUTHN_RP_ID.
    let rp_id = std::env::var("MOLTIS_WEBAUTHN_RP_ID").unwrap_or_else(|_| "localhost".into());
    let default_scheme = if config.tls.enabled {
        "https"
    } else {
        "http"
    };
    let rp_origin_str = std::env::var("MOLTIS_WEBAUTHN_ORIGIN")
        .unwrap_or_else(|_| format!("{default_scheme}://{rp_id}:{port}"));
    let webauthn_state = match webauthn_rs::prelude::Url::parse(&rp_origin_str) {
        Ok(rp_origin) => match crate::auth_webauthn::WebAuthnState::new(&rp_id, &rp_origin) {
            Ok(wa) => Some(Arc::new(wa)),
            Err(e) => {
                tracing::warn!("failed to init WebAuthn: {e}");
                None
            },
        },
        Err(e) => {
            tracing::warn!("invalid WebAuthn origin URL '{rp_origin_str}': {e}");
            None
        },
    };

    // If MOLTIS_PASSWORD is set and no password in DB yet, migrate it.
    if let Some(ref pw) = password
        && !credential_store.is_setup_complete()
    {
        info!("migrating MOLTIS_PASSWORD env var to credential store");
        if let Err(e) = credential_store.set_initial_password(pw).await {
            tracing::warn!("failed to migrate env password: {e}");
        }
    }

    crate::message_log_store::SqliteMessageLog::init(&db_pool)
        .await
        .expect("failed to init message_log table");
    let message_log: Arc<dyn moltis_channels::message_log::MessageLog> = Arc::new(
        crate::message_log_store::SqliteMessageLog::new(db_pool.clone()),
    );

    // Migrate from projects.toml if it exists.
    let config_dir =
        moltis_config::config_dir().unwrap_or_else(|| std::path::PathBuf::from(".moltis"));
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
        backend: config.tools.exec.sandbox.backend.clone(),
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
            if let Some(ref image) = entry.sandbox_image {
                sandbox_router
                    .set_image_override(&entry.key, image.clone())
                    .await;
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

    let is_localhost = matches!(bind, "127.0.0.1" | "::1" | "localhost");
    let state = GatewayState::with_options(
        resolved_auth,
        services,
        Arc::clone(&approval_manager),
        Some(Arc::clone(&sandbox_router)),
        Some(Arc::clone(&credential_store)),
        webauthn_state,
        is_localhost,
    );

    // Generate a one-time setup code if setup is pending and auth is not disabled.
    let setup_code_display =
        if !credential_store.is_setup_complete() && !credential_store.is_auth_disabled() {
            let code = crate::auth_routes::generate_setup_code();
            *state.setup_code.write().await = Some(secrecy::Secret::new(code.clone()));
            Some(code)
        } else {
            None
        };

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
        format!("sandbox: {} backend", sandbox_router.backend_name()),
        format!(
            "config: {}",
            moltis_config::find_or_default_config_path().display()
        ),
        format!("data: {}", data_dir.display()),
    ];
    // Hint about Apple Container on macOS when using Docker.
    #[cfg(target_os = "macos")]
    if sandbox_router.backend_name() == "docker" {
        lines.push(
            "hint: install Apple Container for VM-isolated sandboxing (see docs/sandbox.md)".into(),
        );
    }
    // Warn when no sandbox backend is available.
    if sandbox_router.backend_name() == "none" {
        lines.push("⚠ no container runtime found; commands run on host".into());
    }
    // Display setup code if one was generated.
    if let Some(ref code) = setup_code_display {
        lines.push(format!(
            "setup code: {code} (enter this in the browser to set your password)"
        ));
    }
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
/// Server-side data injected into every page as `window.__MOLTIS__`
/// (gon pattern — see CLAUDE.md § Server-Injected Data).
///
/// Add new fields here when the frontend needs data at page load
/// without an async fetch. Fields must not depend on the request
/// (no cookies, no session — use `/api/auth/status` for that).
#[cfg(feature = "web-ui")]
#[derive(serde::Serialize)]
struct GonData {
    identity: moltis_config::ResolvedIdentity,
}

#[cfg(feature = "web-ui")]
async fn build_gon_data(gw: &GatewayState) -> GonData {
    let identity = gw
        .services
        .onboarding
        .identity_get()
        .await
        .ok()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    GonData { identity }
}

#[cfg(feature = "web-ui")]
async fn api_gon_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(build_gon_data(&state.gateway).await)
}

#[cfg(feature = "web-ui")]
async fn spa_fallback(State(state): State<AppState>, uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path();
    if path.starts_with("/assets/") || path.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    let raw = read_asset("index.html")
        .and_then(|b| String::from_utf8(b).ok())
        .unwrap_or_default();

    // Build server-side data blob (gon pattern) injected into <head>.
    let gon = build_gon_data(&state.gateway).await;
    let gon_script = format!(
        "<script>window.__MOLTIS__={};</script>",
        serde_json::to_string(&gon).unwrap_or_else(|_| "{}".into()),
    );

    let body = if is_dev_assets() {
        // Dev: no versioned URLs, just serve directly with no-cache
        raw.replace("__BUILD_TS__", "dev")
    } else {
        // Production: inject content-hash versioned URLs for immutable caching
        static HASH: std::sync::LazyLock<String> = std::sync::LazyLock::new(asset_content_hash);
        let versioned = format!("/assets/v/{}/", *HASH);
        raw.replace("__BUILD_TS__", &HASH)
            .replace("/assets/", &versioned)
    };

    // Inject gon data into <head> so it's available before any module scripts run.
    let body = body.replace("</head>", &format!("{gon_script}\n</head>"));

    ([("cache-control", "no-cache, no-store")], Html(body)).into_response()
}

#[cfg(feature = "web-ui")]
async fn api_bootstrap_handler(State(state): State<AppState>) -> impl IntoResponse {
    let gw = &state.gateway;
    let (channels, sessions, models, projects, wizard_status) = tokio::join!(
        gw.services.channel.status(),
        gw.services.session.list(),
        gw.services.model.list(),
        gw.services.project.list(),
        gw.services.onboarding.wizard_status(),
    );
    let onboarded = wizard_status
        .as_ref()
        .ok()
        .and_then(|v| v.get("onboarded"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let identity = gw.services.agent.identity_get().await.ok();
    let sandbox = if let Some(ref router) = state.gateway.sandbox_router {
        let default_image = router.default_image().await;
        serde_json::json!({
            "backend": router.backend_name(),
            "os": std::env::consts::OS,
            "default_image": default_image,
        })
    } else {
        serde_json::json!({
            "backend": "none",
            "os": std::env::consts::OS,
            "default_image": moltis_tools::sandbox::DEFAULT_SANDBOX_IMAGE,
        })
    };
    Json(serde_json::json!({
        "channels": channels.ok(),
        "sessions": sessions.ok(),
        "models": models.ok(),
        "projects": projects.ok(),
        "onboarded": onboarded,
        "identity": identity,
        "sandbox": sandbox,
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

/// List cached tool images.
#[cfg(feature = "web-ui")]
async fn api_cached_images_handler() -> impl IntoResponse {
    let builder = moltis_tools::image_cache::DockerImageBuilder::new();
    match builder.list_cached().await {
        Ok(images) => Json(serde_json::json!({ "images": images })).into_response(),
        Err(e) => {
            let msg = e.to_string();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        },
    }
}

/// Delete a specific cached tool image.
#[cfg(feature = "web-ui")]
async fn api_delete_cached_image_handler(Path(tag): Path<String>) -> impl IntoResponse {
    let builder = moltis_tools::image_cache::DockerImageBuilder::new();
    // The tag comes URL-encoded; the path captures "moltis-cache/skill:hash" as a single segment.
    let full_tag = if tag.starts_with("moltis-cache/") {
        tag
    } else {
        format!("moltis-cache/{tag}")
    };
    match builder.remove_cached(&full_tag).await {
        Ok(()) => Json(serde_json::json!({ "ok": true })).into_response(),
        Err(e) => {
            let msg = e.to_string();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        },
    }
}

/// Prune all cached tool images.
#[cfg(feature = "web-ui")]
async fn api_prune_cached_images_handler() -> impl IntoResponse {
    let builder = moltis_tools::image_cache::DockerImageBuilder::new();
    match builder.prune_all().await {
        Ok(count) => Json(serde_json::json!({ "pruned": count })).into_response(),
        Err(e) => {
            let msg = e.to_string();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response()
        },
    }
}

/// Check which packages already exist in a base image.
///
/// Runs `dpkg -s <pkg>` and `which <pkg>` inside the base image to detect
/// packages that are already installed. Returns a map of package name to
/// boolean (true = already present).
#[cfg(feature = "web-ui")]
async fn api_check_packages_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let base = body
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("ubuntu:25.10")
        .trim()
        .to_string();
    let packages: Vec<String> = body
        .get("packages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    if packages.is_empty() {
        return Json(serde_json::json!({ "found": {} })).into_response();
    }

    // Build a shell command that checks each package via dpkg -s or which.
    let checks: Vec<String> = packages
        .iter()
        .map(|pkg| {
            format!(
                r#"if dpkg -s '{pkg}' >/dev/null 2>&1 || command -v '{pkg}' >/dev/null 2>&1; then echo "FOUND:{pkg}"; fi"#
            )
        })
        .collect();
    let script = checks.join("\n");

    let output = tokio::process::Command::new("docker")
        .args(["run", "--rm", "--entrypoint", "sh", &base, "-c", &script])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let mut found = serde_json::Map::new();
            for pkg in &packages {
                let present = stdout.lines().any(|l| l.trim() == format!("FOUND:{pkg}"));
                found.insert(pkg.clone(), serde_json::Value::Bool(present));
            }
            Json(serde_json::json!({ "found": found })).into_response()
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Get the current default sandbox image.
#[cfg(feature = "web-ui")]
async fn api_get_default_image_handler(State(state): State<AppState>) -> impl IntoResponse {
    let image = if let Some(ref router) = state.gateway.sandbox_router {
        router.default_image().await
    } else {
        moltis_tools::sandbox::DEFAULT_SANDBOX_IMAGE.to_string()
    };
    Json(serde_json::json!({ "image": image }))
}

/// Set the default sandbox image.
#[cfg(feature = "web-ui")]
async fn api_set_default_image_handler(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let image = body.get("image").and_then(|v| v.as_str()).map(|s| s.trim());

    if let Some(ref router) = state.gateway.sandbox_router {
        let value = image.filter(|s| !s.is_empty()).map(String::from);
        router.set_global_image(value.clone()).await;
        let effective = router.default_image().await;
        Json(serde_json::json!({ "image": effective })).into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "no sandbox backend available" })),
        )
            .into_response()
    }
}

/// Build a custom image from a base + apt packages.
#[cfg(feature = "web-ui")]
async fn api_build_image_handler(Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let base = body
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("ubuntu:25.10")
        .trim();
    let packages: Vec<&str> = body
        .get("packages")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name is required" })),
        )
            .into_response();
    }
    if packages.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "packages list is empty" })),
        )
            .into_response();
    }

    // Validate name: only allow alphanumeric, dash, underscore
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "name must be alphanumeric, dash, or underscore" })),
        )
            .into_response();
    }

    let pkg_list = packages.join(" ");
    let dockerfile_contents = format!(
        "FROM {base}\nRUN apt-get update && apt-get install -y {pkg_list} && rm -rf /var/lib/apt/lists/*\n"
    );

    let tmp_dir = std::env::temp_dir().join(format!("moltis-build-{}", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let dockerfile_path = tmp_dir.join("Dockerfile");
    if let Err(e) = std::fs::write(&dockerfile_path, &dockerfile_contents) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }

    let builder = moltis_tools::image_cache::DockerImageBuilder::new();
    let result = builder.ensure_image(name, &dockerfile_path, &tmp_dir).await;
    let _ = std::fs::remove_dir_all(&tmp_dir);
    match result {
        Ok(tag) => Json(serde_json::json!({ "tag": tag })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
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
        if file_path.starts_with(dir)
            && let Ok(bytes) = std::fs::read(&file_path)
        {
            return Some(bytes);
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
