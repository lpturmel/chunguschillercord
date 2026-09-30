use axum::{Router, routing::get};
use error::{Error, Result};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod config;
mod database;
mod discord;
mod error;
mod keystones;
mod league;
mod valorant;

#[tokio::main]
async fn main() -> Result<()> {
    install_rustls_crypto_provider();
    dotenv::dotenv().ok();
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--keystones") {
        return keystones::run(&args[1..]).await;
    }

    if args.first().map(String::as_str) == Some("--discord-registration-info") {
        if args.len() != 1 {
            return Err(Error::Config(
                "--discord-registration-info takes no arguments".into(),
            ));
        }
        return discord::registration_info().await;
    }

    let config = config::load()?;
    let database = database::BotDatabase::initialize_from_env().await?;
    let league_service = league::initialize_from_env(
        config.schedule.clone(),
        config.league_players,
        database.clone(),
    )
    .await?;
    let valorant_service =
        valorant::initialize_from_env(config.schedule, config.valorant_player, database.clone())?;
    let keystone_service = keystones::KeystoneService::initialize(database)?;
    let interaction_routes =
        discord::initialize_from_env(league_service, valorant_service, keystone_service).await?;
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse::<u16>()
        .map_err(|_| Error::Config("PORT must be an integer from 0 to 65535".to_string()))?;
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let app = Router::new()
        .route("/health", get(health))
        .merge(interaction_routes);
    let listener = TcpListener::bind(address).await?;

    info!(%address, "chunguschillercord Axum server is listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn install_rustls_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,chunguschillercord=info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true)
        .init();
}

async fn health() -> &'static str {
    "ok"
}
