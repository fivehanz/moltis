use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::{
    fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter,
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

fn init_telemetry(cli: &Cli) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cli.log_level));

    if cli.json_logs {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().json().with_target(true).with_thread_ids(false))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt::layer().with_target(false).with_thread_ids(false).with_ansi(true))
            .init();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    init_telemetry(&cli);

    info!(version = env!("CARGO_PKG_VERSION"), "moltis starting");

    match cli.command {
        Commands::Gateway { bind, port } => {
            moltis_gateway::server::start_gateway(&bind, port).await
        }
        Commands::Agent { message, .. } => {
            let result = moltis_agents::runner::run_agent("default", "main", &message).await?;
            println!("{result}");
            Ok(())
        }
        Commands::Onboard => moltis_onboarding::wizard::run_onboarding().await,
        _ => {
            eprintln!("command not yet implemented");
            Ok(())
        }
    }
}
