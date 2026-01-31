mod auth_commands;

use {
    clap::{Parser, Subcommand},
    moltis_gateway::logs::{LogBroadcastLayer, LogBuffer},
    tracing::info,
    tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt},
};

#[derive(Parser)]
#[command(name = "moltis", about = "Moltis — personal AI gateway")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long, global = true, default_value = "info")]
    log_level: String,

    /// Output logs as JSON instead of human-readable.
    #[arg(long, global = true, default_value_t = false)]
    json_logs: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the gateway server.
    Gateway {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 18789)]
        port: u16,
    },
    /// Invoke an agent directly.
    Agent {
        #[arg(short, long)]
        message: String,
        #[arg(long)]
        thinking: Option<String>,
    },
    /// Channel management.
    Channels {
        #[command(subcommand)]
        action: ChannelAction,
    },
    /// Send a message.
    Send {
        #[arg(long)]
        to: String,
        #[arg(short, long)]
        message: String,
    },
    /// Session management.
    Sessions {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Configuration management.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List available models.
    Models,
    /// Interactive onboarding wizard.
    Onboard,
    /// Config validation and migration.
    Doctor,
    /// Authentication management for OAuth providers.
    Auth {
        #[command(subcommand)]
        action: auth_commands::AuthAction,
    },
}

#[derive(Subcommand)]
enum ChannelAction {
    Status,
    Login,
    Logout,
}

#[derive(Subcommand)]
enum SessionAction {
    List,
    Clear { key: String },
    History { key: String },
}

#[derive(Subcommand)]
enum ConfigAction {
    Get { key: Option<String> },
    Set { key: String, value: String },
    Edit,
}

/// Initialise tracing and optionally attach a [`LogBroadcastLayer`] that
/// captures events into an in-memory ring buffer for the web UI.
fn init_telemetry(cli: &Cli, log_buffer: Option<LogBuffer>) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cli.log_level));

    let registry = tracing_subscriber::registry().with(filter);

    // Optionally attach the in-memory capture layer.
    let log_layer = log_buffer.map(LogBroadcastLayer::new);

    if cli.json_logs {
        registry
            .with(fmt::layer().json().with_target(true).with_thread_ids(false))
            .with(log_layer)
            .init();
    } else {
        registry
            .with(
                fmt::layer()
                    .with_target(false)
                    .with_thread_ids(false)
                    .with_ansi(true),
            )
            .with(log_layer)
            .init();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    // Create the log buffer only for the gateway command so the web UI can
    // display captured log entries.
    let log_buffer = if matches!(cli.command, Commands::Gateway { .. }) {
        Some(LogBuffer::default())
    } else {
        None
    };

    init_telemetry(&cli, log_buffer.clone());

    info!(version = env!("CARGO_PKG_VERSION"), "moltis starting");

    match cli.command {
        Commands::Gateway { bind, port } => {
            moltis_gateway::server::start_gateway(&bind, port, log_buffer).await
        },
        Commands::Agent { message, .. } => {
            let result = moltis_agents::runner::run_agent("default", "main", &message).await?;
            println!("{result}");
            Ok(())
        },
        Commands::Onboard => moltis_onboarding::wizard::run_onboarding().await,
        Commands::Auth { action } => auth_commands::handle_auth(action).await,
        _ => {
            eprintln!("command not yet implemented");
            Ok(())
        },
    }
}
