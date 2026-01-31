use {
    anyhow::Result,
    clap::Subcommand,
    moltis_oauth::{
        CallbackServer, OAuthFlow, TokenStore, callback_port, device_flow, load_oauth_config,
    },
};

#[derive(Subcommand)]
pub enum AuthAction {
    /// Log in to a provider via OAuth.
    Login {
        /// Provider name (e.g. "openai-codex").
        #[arg(long)]
        provider: String,
    },
    /// Show authentication status for all providers.
    Status,
    /// Log out from a provider.
    Logout {
        /// Provider name (e.g. "openai-codex").
        #[arg(long)]
        provider: String,
    },
}

pub async fn handle_auth(action: AuthAction) -> Result<()> {
    match action {
        AuthAction::Login { provider } => login(&provider).await,
        AuthAction::Status => status(),
        AuthAction::Logout { provider } => logout(&provider),
    }
}

async fn login(provider: &str) -> Result<()> {
    let config = load_oauth_config(provider)
        .ok_or_else(|| anyhow::anyhow!("unknown OAuth provider: {provider}"))?;

    if config.device_flow {
        return login_device_flow(provider, &config).await;
    }

    let port = callback_port(&config);
    let flow = OAuthFlow::new(config);
    let req = flow.start();

    println!("Opening browser for authentication...");
    if open::that(&req.url).is_err() {
        println!("Could not open browser. Please visit:\n{}", req.url);
    }

    println!("Waiting for callback on http://127.0.0.1:{port}/auth/callback ...");
    let code = CallbackServer::wait_for_code(port, req.state).await?;

    println!("Exchanging code for tokens...");
    let tokens = flow.exchange(&code, &req.pkce.verifier).await?;

    let store = TokenStore::new();
    store.save(provider, &tokens)?;

    println!("Successfully logged in to {provider}");
    Ok(())
}

async fn login_device_flow(provider: &str, config: &moltis_oauth::OAuthConfig) -> Result<()> {
    let client = reqwest::Client::new();

    // Build extra headers for providers that need them (e.g. Kimi Code).
    let extra_headers = build_provider_headers(provider);
    let extra = extra_headers.as_ref();

    let device_resp = device_flow::request_device_code_with_headers(&client, config, extra).await?;

    // Prefer verification_uri_complete (auto-includes user code).
    let open_url = device_resp
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&device_resp.verification_uri);

    println!("Opening browser for device authorization...");
    println!("Your code: {}", device_resp.user_code);
    if open::that(open_url).is_err() {
        println!("Could not open browser. Please visit:\n{open_url}");
    }

    println!("Waiting for authorization...");
    let tokens = device_flow::poll_for_token_with_headers(
        &client,
        config,
        &device_resp.device_code,
        device_resp.interval,
        extra,
    )
    .await?;

    let store = TokenStore::new();
    store.save(provider, &tokens)?;

    println!("Successfully logged in to {provider}");
    Ok(())
}

/// Build provider-specific extra headers for the device flow.
fn build_provider_headers(provider: &str) -> Option<reqwest::header::HeaderMap> {
    match provider {
        "kimi-code" => Some(moltis_oauth::kimi_headers()),
        _ => None,
    }
}

fn status() -> Result<()> {
    let store = TokenStore::new();
    let providers = store.list();
    if providers.is_empty() {
        println!("No authenticated providers.");
        return Ok(());
    }
    for provider in providers {
        if let Some(tokens) = store.load(&provider) {
            let expiry = tokens.expires_at.map_or("unknown".to_string(), |ts| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if ts > now {
                    let remaining = ts - now;
                    let hours = remaining / 3600;
                    let mins = (remaining % 3600) / 60;
                    format!("valid ({hours}h {mins}m remaining)")
                } else {
                    "expired".to_string()
                }
            });
            println!("{provider} [{expiry}]");
        }
    }
    Ok(())
}

fn logout(provider: &str) -> Result<()> {
    let store = TokenStore::new();
    store.delete(provider)?;
    println!("Logged out from {provider}");
    Ok(())
}
