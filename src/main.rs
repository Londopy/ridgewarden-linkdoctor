use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use ridgewarden_client::util::{norm_node, valid_node};
use serde_json::json;

use ridgewarden_linkdoctor::App;
use ridgewarden_linkdoctor::config::{Config, EXAMPLE};
use ridgewarden_linkdoctor::{spawn_loops, web};

#[derive(Parser)]
#[command(version, about = "Link Doctor for AREDN meshes (a ridgewarden app)")]
struct Cli {
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/linkdoctor.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config
    Init,
    /// Follow the core and serve the links page
    Run,
    /// Note something about a site or link; it shows on the charts, signed by this site's core
    Note {
        /// the site's node name
        site: String,
        text: String,
        /// the other end, for a note about one link
        #[arg(long)]
        to: Option<String>,
        /// a change at the radio (realigned, new cable or hardware): drift is measured from here
        #[arg(long)]
        change: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ridgewarden_linkdoctor=info,ridgewarden_client=info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init => {
            if cli.config.exists() {
                bail!(
                    "{} already exists; not overwriting it",
                    cli.config.display()
                );
            }
            if let Some(d) = cli.config.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&cli.config, EXAMPLE)?;
            println!("Wrote {}", cli.config.display());
            Ok(())
        }
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(run(cfg))
        }
        Cmd::Note {
            site,
            text,
            to,
            change,
        } => {
            let site = norm_node(&site);
            let to = to.map(|t| norm_node(&t));
            for n in std::iter::once(&site).chain(&to) {
                if !valid_node(n) {
                    bail!("{n:?} is not a node name");
                }
            }
            if text.trim().is_empty() {
                bail!("the note is empty");
            }
            let app = App::new(Config::load(&cli.config)?, true)?;
            let payload = json!({"node": site, "other": to, "text": text, "change": change});
            tokio::runtime::Runtime::new()?.block_on(app.core.append("link.note", payload))?;
            println!("Note added.");
            Ok(())
        }
    }
}

async fn run(cfg: Config) -> Result<()> {
    let listen = cfg.listen;
    let app = Arc::new(App::new(cfg, false)?);
    tracing::info!(
        "ridgewarden-linkdoctor {} following {}, page on http://{listen}",
        env!("CARGO_PKG_VERSION"),
        app.core.base()
    );
    spawn_loops(app.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
