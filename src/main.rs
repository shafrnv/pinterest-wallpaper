mod auth;
mod cache;
mod config;
mod pinterest;
mod wallpaper;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use auth::AuthStore;
use config::Config;
use pinterest::oauth;

#[derive(Parser)]
#[command(
    name = "pinterest-wallpaper",
    about = "Pinterest feed as a Hyprland wallpaper"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the wallpaper (also the default when no command is given)
    Run,
    /// Log in via official Pinterest OAuth (boards / pins)
    LoginOauth,
    /// Log in via browser cookies (real home feed)
    LoginSession,
    /// Clear stored credentials
    Logout {
        #[arg(long)]
        oauth_only: bool,
        #[arg(long)]
        session_only: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => cmd_run(),
        Command::LoginOauth => cmd_login_oauth(),
        Command::LoginSession => cmd_login_session(),
        Command::Logout {
            oauth_only,
            session_only,
        } => cmd_logout(oauth_only, session_only),
    }
}

fn cmd_run() -> Result<()> {
    let cfg = Config::load()?;
    wallpaper::run(cfg)
}

fn cmd_login_oauth() -> Result<()> {
    let cfg = Config::load()?;
    let oauth = cfg
        .oauth
        .as_ref()
        .context("set [oauth] client_id and client_secret in config.toml")?;
    let tokens = oauth::login(oauth)?;
    let mut store = AuthStore::load()?;
    store.oauth = Some(tokens);
    store.save()?;
    println!("OAuth tokens saved to {}", Config::config_dir()?.display());
    Ok(())
}

fn cmd_login_session() -> Result<()> {
    let session = AuthStore::prompt_session_login()?;
    let mut store = AuthStore::load()?;
    store.session = Some(session);
    store.save()?;
    println!("Session cookies saved to {}", Config::config_dir()?.display());
    Ok(())
}

fn cmd_logout(oauth_only: bool, session_only: bool) -> Result<()> {
    let mut store = AuthStore::load()?;
    if oauth_only && !session_only {
        store.clear_oauth()?;
        println!("cleared OAuth tokens");
    } else if session_only && !oauth_only {
        store.clear_session()?;
        println!("cleared session cookies");
    } else {
        store.clear_all()?;
        println!("cleared all credentials");
    }
    Ok(())
}
