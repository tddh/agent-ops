mod connect;
mod protocol;
mod terminal;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "agent-ops", about = "AI Agent 远程运维 CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, default_value = "config/hosts.yaml")]
    hosts_file: String,

    #[arg(long, default_value = "config/ca.crt")]
    ca_cert: String,
}

#[derive(Subcommand)]
enum Commands {
    Connect {
        host: String,

        #[arg(long, default_value = "agent-ops")]
        session: String,

        #[arg(long)]
        pane: Option<String>,

        #[arg(long)]
        readonly: bool,
    },

    List {
        host: String,
    },
}

fn load_host_config(
    hosts_file: &str,
    host_name: &str,
) -> anyhow::Result<agent_ops_core::HostConfig> {
    let contents = std::fs::read_to_string(hosts_file)?;
    let registry: agent_ops_core::HostRegistry = serde_yaml::from_str(&contents)?;
    registry
        .hosts
        .into_iter()
        .find(|h| h.name == host_name)
        .ok_or_else(|| anyhow::anyhow!("host '{}' not found in {}", host_name, hosts_file))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Connect {
            host,
            session,
            pane,
            readonly,
        } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            let pane = match pane {
                Some(p) => p,
                None => connect::find_lowest_pane(&config, &cli.ca_cert, &session).await?,
            };
            connect::connect(&config, &cli.ca_cert, &session, &pane, readonly).await
        }
        Commands::List { host } => {
            let config = load_host_config(&cli.hosts_file, &host)?;
            connect::list_sessions(&config, &cli.ca_cert).await
        }
    }
}
