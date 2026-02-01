mod auth_commands;
mod hooks_commands;

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
    /// Skill management.
    Skills {
        #[command(subcommand)]
        action: SkillAction,
    },
    /// Hook management.
    Hooks {
        #[command(subcommand)]
        action: hooks_commands::HookAction,
    },
    /// Install the Moltis CA certificate into the system trust store.
    #[cfg(feature = "tls")]
    TrustCa,
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

#[derive(Subcommand)]
enum SkillAction {
    /// List all discovered skills.
    List,
    /// Install a skill from a GitHub repository (owner/repo format).
    Add {
        /// Source in owner/repo format (e.g. vercel-labs/agent-skills).
        source: String,
    },
    /// Remove an installed repo and all its skills.
    Remove {
        /// Source in owner/repo format.
        source: String,
    },
    /// Show details about a skill.
    Info {
        /// Skill name.
        name: String,
    },
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

#[cfg(feature = "tls")]
async fn trust_ca() -> anyhow::Result<()> {
    let cert_dir = moltis_gateway::tls::cert_dir()?;
    let ca_path = cert_dir.join("ca.pem");

    if !ca_path.exists() {
        eprintln!(
            "CA certificate not found at {}. Start the gateway first to generate certificates.",
            ca_path.display()
        );
        return Ok(());
    }

    eprintln!("Installing CA certificate: {}", ca_path.display());

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("security")
            .args([
                "add-trusted-cert",
                "-r",
                "trustRoot",
                "-k",
                &format!(
                    "{}/Library/Keychains/login.keychain-db",
                    std::env::var("HOME").unwrap_or_default()
                ),
            ])
            .arg(&ca_path)
            .status()?;
        if status.success() {
            eprintln!(
                "CA certificate installed successfully. Restart your browser to pick up the change."
            );
        } else {
            eprintln!("Failed to install CA certificate (exit code: {})", status);
        }
    }

    #[cfg(target_os = "linux")]
    {
        let dest = std::path::PathBuf::from("/usr/local/share/ca-certificates/moltis-ca.crt");
        eprintln!("Copying CA to {} (may require sudo)", dest.display());
        let status = std::process::Command::new("sudo")
            .args(["cp"])
            .arg(&ca_path)
            .arg(&dest)
            .status()?;
        if status.success() {
            let update = std::process::Command::new("sudo")
                .arg("update-ca-certificates")
                .status()?;
            if update.success() {
                eprintln!("CA certificate installed successfully.");
            } else {
                eprintln!("update-ca-certificates failed (exit code: {})", update);
            }
        } else {
            eprintln!("Failed to copy CA certificate (exit code: {})", status);
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        eprintln!(
            "Automatic trust installation is not supported on this OS.\n\
             Manually import the CA certificate from: {}",
            ca_path.display()
        );
    }

    Ok(())
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
        Commands::Skills { action } => handle_skills(action).await,
        Commands::Hooks { action } => hooks_commands::handle_hooks(action).await,
        #[cfg(feature = "tls")]
        Commands::TrustCa => trust_ca().await,
        _ => {
            eprintln!("command not yet implemented");
            Ok(())
        },
    }
}

async fn handle_skills(action: SkillAction) -> anyhow::Result<()> {
    use moltis_skills::{
        discover::FsSkillDiscoverer,
        install,
        registry::{InMemoryRegistry, SkillRegistry},
    };

    let cwd = std::env::current_dir()?;
    let search_paths = FsSkillDiscoverer::default_paths(&cwd);
    let discoverer = FsSkillDiscoverer::new(search_paths);

    match action {
        SkillAction::List => {
            let registry = InMemoryRegistry::from_discoverer(&discoverer).await?;
            let skills = registry.list_skills().await?;
            if skills.is_empty() {
                println!("No skills found.");
            } else {
                for skill in &skills {
                    let source = skill
                        .source
                        .as_ref()
                        .map(|s| format!("{s:?}"))
                        .unwrap_or_default();
                    println!("  {} — {} [{}]", skill.name, skill.description, source);
                }
            }
        },
        SkillAction::Add { source } => {
            let install_dir = install::default_install_dir()?;
            let skills = install::install_skill(&source, &install_dir).await?;
            for meta in &skills {
                println!("Installed skill '{}': {}", meta.name, meta.description);
            }
        },
        SkillAction::Remove { source } => {
            let install_dir = install::default_install_dir()?;
            install::remove_repo(&source, &install_dir).await?;
            println!("Removed repo '{source}' and all its skills.");
        },
        SkillAction::Info { name } => {
            let registry = InMemoryRegistry::from_discoverer(&discoverer).await?;
            let content = registry.load_skill(&name).await?;
            let meta = &content.metadata;
            println!("Name:        {}", meta.name);
            println!("Description: {}", meta.description);
            if let Some(ref license) = meta.license {
                println!("License:     {license}");
            }
            if !meta.allowed_tools.is_empty() {
                println!("Tools:       {}", meta.allowed_tools.join(", "));
            }
            println!("Path:        {}", meta.path.display());
            println!("Source:      {:?}", meta.source);
            println!("\n{}", content.body);
        },
    }

    Ok(())
}
