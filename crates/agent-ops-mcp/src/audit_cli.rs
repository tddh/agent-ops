use crate::audit;
use clap::Parser;
use std::path::PathBuf;

use crate::resolve_audit_db_path;

pub async fn run_audit_command() -> anyhow::Result<()> {
    #[derive(Parser)]
    struct AuditCli {
        #[command(subcommand)]
        command: AuditCommand,
    }

    #[derive(clap::Subcommand)]
    enum AuditCommand {
        Query {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            host: Option<String>,
            #[arg(long)]
            action: Option<String>,
            #[arg(long)]
            agent: Option<String>,
            #[arg(long)]
            since: Option<String>,
            #[arg(long)]
            until: Option<String>,
            #[arg(long)]
            success: Option<bool>,
            #[arg(long, default_value = "50")]
            limit: u32,
            #[arg(long, default_value = "table")]
            format: String,
        },
        Stats {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            since: Option<String>,
        },
        Cleanup {
            #[arg(long)]
            db: Option<PathBuf>,
            #[arg(long)]
            older_than: Option<u32>,
            #[arg(long)]
            max_size: Option<u64>,
        },
    }

    let cli = AuditCli::parse_from(
        std::iter::once("agent-ops-mcp".to_string()).chain(std::env::args().skip(2)),
    );
    match cli.command {
        AuditCommand::Query {
            db,
            host,
            action,
            agent,
            since,
            until,
            success,
            limit,
            format,
        } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let fmt = match format.as_str() {
                "json" => audit::query::OutputFormat::Json,
                "jsonl" => audit::query::OutputFormat::Jsonl,
                _ => audit::query::OutputFormat::Table,
            };
            let params = audit::query::QueryParams {
                host,
                action,
                agent,
                since,
                until,
                success,
                limit: Some(limit),
            };
            let result = audit_db.query(params, fmt).await?;
            println!("{}", result);
        }
        AuditCommand::Stats { db, since } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let result = audit_db.stats(since).await?;
            println!("{}", result);
        }
        AuditCommand::Cleanup {
            db,
            older_than,
            max_size,
        } => {
            let db_path = resolve_audit_db_path(db);
            let audit_db = audit::AuditDb::open(&db_path)?;
            let days = older_than.unwrap_or(90);
            let size = max_size.unwrap_or(500);
            audit_db.cleanup(days, size).await?;
            println!("Cleanup completed.");
        }
    }
    Ok(())
}
