use crate::{
    error::{Error, Result},
    keystones::KeystoneService,
    league::LeagueService,
    valorant::ValorantService,
};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use ed25519_dalek::{Signature, VerifyingKey};
use reqwest::Client as HttpClient;
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{error, info, warn};
use twilight_http::Client as DiscordClient;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ApplicationMarker, GuildMarker, UserMarker},
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MANUAL_POLL_COOLDOWN: Duration = Duration::from_secs(60);
const SIGNATURE_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const EPHEMERAL_FLAG: u64 = 1 << 6;
const PING: u8 = 1;
const APPLICATION_COMMAND: u8 = 2;
const PONG_RESPONSE: u8 = 1;
const MESSAGE_RESPONSE: u8 = 4;
const DEFERRED_MESSAGE_RESPONSE: u8 = 5;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Command {
    League,
    Valorant,
    Keys,
}

impl Command {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "league" => Some(Self::League),
            "valorant" => Some(Self::Valorant),
            "keys" => Some(Self::Keys),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::League => "league",
            Self::Valorant => "valorant",
            Self::Keys => "keys",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::League => "Show your League ranked recap",
            Self::Valorant => "Show the configured Valorant rank",
            Self::Keys => "List available allowlisted WoW keystone snapshots and their freshness",
        }
    }
}

struct InteractionConfig {
    application_id: Id<ApplicationMarker>,
    public_key: VerifyingKey,
    guild_id: Option<Id<GuildMarker>>,
}

impl InteractionConfig {
    fn from_env() -> Result<Option<Self>> {
        let application_id = env_value("DISCORD_APPLICATION_ID");
        let public_key = env_value("DISCORD_PUBLIC_KEY");
        let guild_id = env_value("DISCORD_GUILD_ID");

        if application_id.is_none() && public_key.is_none() && guild_id.is_none() {
            return Ok(None);
        }

        Ok(Some(Self {
            application_id: parse_application_id(
                application_id.as_deref().ok_or_else(|| {
                    Error::Config("Missing env var DISCORD_APPLICATION_ID".into())
                })?,
            )?,
            public_key: parse_public_key(
                public_key
                    .as_deref()
                    .ok_or_else(|| Error::Config("Missing env var DISCORD_PUBLIC_KEY".into()))?,
            )?,
            guild_id: guild_id.as_deref().map(parse_guild_id).transpose()?,
        }))
    }
}

struct InteractionState {
    league: Option<Arc<LeagueService>>,
    valorant: Option<Arc<ValorantService>>,
    keys: Option<Arc<KeystoneService>>,
    guild_id: Option<Id<GuildMarker>>,
    discord_client: Arc<DiscordClient>,
    application_id: Id<ApplicationMarker>,
    public_key: VerifyingKey,
    cooldowns: Mutex<HashMap<(Command, Id<UserMarker>), Instant>>,
}

pub(crate) async fn initialize_from_env(
    league: Option<Arc<LeagueService>>,
    valorant: Option<Arc<ValorantService>>,
    mut keys: Option<Arc<KeystoneService>>,
) -> Result<Router> {
    if league.is_none() && valorant.is_none() && keys.is_none() {
        return Ok(Router::new());
    }
    let Some(config) = InteractionConfig::from_env()? else {
        info!(
            "Discord commands are disabled; DISCORD_APPLICATION_ID and DISCORD_PUBLIC_KEY are not set"
        );
        return Ok(Router::new());
    };
    // Keystone records are shared only inside the explicitly configured guild.
    if keys.is_some() && config.guild_id.is_none() {
        warn!("/keys is disabled until DISCORD_GUILD_ID is configured");
        keys = None;
    }
    if league.is_none() && valorant.is_none() && keys.is_none() {
        return Ok(Router::new());
    }
    let bot_token = require_env("DISCORD_BOT_TOKEN")?;
    let commands = [
        league.as_ref().map(|_| Command::League),
        valorant.as_ref().map(|_| Command::Valorant),
        keys.as_ref().map(|_| Command::Keys),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if register_on_startup(env_value("DISCORD_REGISTER_COMMANDS_ON_STARTUP").as_deref())? {
        register_commands(&bot_token, &config, &commands).await?;
    } else {
        info!(
            "Automatic Discord command registration is disabled; existing registrations are retained"
        );
    }

    let state = Arc::new(InteractionState {
        league,
        valorant,
        keys,
        guild_id: config.guild_id,
        discord_client: Arc::new(DiscordClient::new(bot_token)),
        application_id: config.application_id,
        public_key: config.public_key,
        cooldowns: Mutex::new(HashMap::new()),
    });
    info!(commands = ?commands.iter().map(|command| command.name()).collect::<Vec<_>>(), "Discord interaction endpoint is enabled");
    Ok(Router::new()
        .route("/discord/interactions", post(handle_interaction))
        .with_state(state))
}

#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    guild_id: Option<String>,
    #[serde(default)]
    data: Option<DiscordInteractionData>,
    #[serde(default)]
    member: Option<DiscordMember>,
    #[serde(default)]
    user: Option<DiscordUser>,
}

impl DiscordInteraction {
    fn author_id(&self) -> Option<Id<UserMarker>> {
        self.member
            .as_ref()
            .map(|member| &member.user)
            .or(self.user.as_ref())
            .and_then(|user| user.id.parse::<u64>().ok())
            .and_then(Id::new_checked)
    }
}

#[derive(Debug, Deserialize)]
struct DiscordInteractionData {
    name: String,
}

#[derive(Debug, Deserialize)]
struct DiscordMember {
    user: DiscordUser,
}

#[derive(Debug, Deserialize)]
struct DiscordUser {
    id: String,
}

async fn handle_interaction(
    State(state): State<Arc<InteractionState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(signature) = header_text(&headers, "X-Signature-Ed25519") else {
        return (StatusCode::UNAUTHORIZED, "missing Discord signature").into_response();
    };
    let Some(timestamp) = header_text(&headers, "X-Signature-Timestamp") else {
        return (StatusCode::UNAUTHORIZED, "missing Discord timestamp").into_response();
    };
    if !verify_request(&state.public_key, signature, timestamp, &body) {
        return (StatusCode::UNAUTHORIZED, "invalid Discord signature").into_response();
    }

    let interaction = match serde_json::from_slice::<DiscordInteraction>(&body) {
        Ok(interaction) => interaction,
        Err(err) => {
            warn!("Discord interaction JSON was invalid: {err}");
            return (StatusCode::BAD_REQUEST, "invalid Discord interaction").into_response();
        }
    };
    if interaction.kind == PING {
        return discord_response(PONG_RESPONSE, None);
    }
    let command = interaction
        .data
        .as_ref()
        .and_then(|data| Command::from_name(&data.name));
    if interaction.kind != APPLICATION_COMMAND || command.is_none() {
        return discord_message(
            "This endpoint handles `/league`, `/valorant` and `/keys`.",
            true,
        );
    }
    let command = command.expect("checked above");
    let Some(discord_user_id) = interaction.author_id() else {
        return discord_message("Discord did not include the invoking user.", true);
    };

    match command {
        Command::League => handle_league(state, interaction, discord_user_id),
        Command::Valorant => handle_valorant(state, interaction, discord_user_id),
        Command::Keys => handle_keys(state, interaction, discord_user_id),
    }
}

fn keys_guild_allowed(
    interaction: &DiscordInteraction,
    configured: Option<Id<GuildMarker>>,
) -> bool {
    configured.is_some_and(|guild| {
        interaction.member.is_some()
            && interaction
                .guild_id
                .as_deref()
                .and_then(|v| v.parse::<u64>().ok())
                == Some(guild.get())
    })
}
fn handle_keys(
    state: Arc<InteractionState>,
    interaction: DiscordInteraction,
    discord_user_id: Id<UserMarker>,
) -> Response {
    if !keys_guild_allowed(&interaction, state.guild_id) {
        return discord_message("Use /keys in the configured server.", true);
    }
    let Some(service) = state.keys.as_ref() else {
        return discord_message("The keystone command is currently disabled.", true);
    };
    let Some(token) = interaction.token else {
        return discord_message("Discord did not include an interaction token.", true);
    };
    if let Some(remaining) = claim_manual_poll(&state.cooldowns, Command::Keys, discord_user_id) {
        return cooldown_message(Command::Keys, remaining);
    }
    let service = Arc::clone(service);
    let discord_client = Arc::clone(&state.discord_client);
    let application_id = state.application_id;
    tokio::spawn(async move {
        let content = match tokio::time::timeout(REQUEST_TIMEOUT, service.prepare_report()).await {
            Ok(Ok(content)) => content,
            // No local-cache fallback after a failed pull: it could misreport available keys.
            Ok(Err(_)) | Err(_) => {
                error!("/keys could not load validated snapshots from Turso");
                "I couldn't load the keystone snapshots right now. Please try again shortly."
                    .to_string()
            }
        };
        if let Err(err) = discord_client
            .interaction(application_id)
            .update_response(&token)
            .content(Some(&content))
            .allowed_mentions(Some(&AllowedMentions::default()))
            .await
        {
            error!("Failed to finish /keys response: {err}");
        }
    });
    discord_response(DEFERRED_MESSAGE_RESPONSE, None)
}

fn handle_league(
    state: Arc<InteractionState>,
    interaction: DiscordInteraction,
    discord_user_id: Id<UserMarker>,
) -> Response {
    let Some(service) = state.league.as_ref() else {
        return discord_message("The League rank worker is currently disabled.", true);
    };
    let Some(player) = service.player_for_discord_user(discord_user_id) else {
        return discord_message(
            "Your Discord account is not linked to a League Riot ID in `config/league-valorant-rank.ron`.",
            true,
        );
    };
    if let Some(remaining) = claim_manual_poll(&state.cooldowns, Command::League, discord_user_id) {
        return cooldown_message(Command::League, remaining);
    }
    let Some(token) = interaction.token else {
        return discord_message("Discord did not include an interaction token.", true);
    };

    let service = Arc::clone(service);
    let discord_client = Arc::clone(&state.discord_client);
    let application_id = state.application_id;
    tokio::spawn(async move {
        let result = service.prepare_report(&player, false).await;
        let (content, allowed_mentions) = match result {
            Ok(report) => {
                info!(riot_id = %player.riot_id, "Prepared manual League rank recap");
                (report.message, report.allowed_mentions)
            }
            Err(err) => {
                error!(riot_id = %player.riot_id, "Manual League rank recap failed: {err}");
                (
                    "I couldn't load your League recap right now. Please try again shortly."
                        .to_string(),
                    AllowedMentions::default(),
                )
            }
        };
        if let Err(err) = discord_client
            .interaction(application_id)
            .update_response(&token)
            .content(Some(&content))
            .allowed_mentions(Some(&allowed_mentions))
            .await
        {
            error!(riot_id = %player.riot_id, "Failed to finish /league response: {err}");
        }
    });
    discord_response(DEFERRED_MESSAGE_RESPONSE, None)
}

fn handle_valorant(
    state: Arc<InteractionState>,
    interaction: DiscordInteraction,
    discord_user_id: Id<UserMarker>,
) -> Response {
    let Some(service) = state.valorant.as_ref() else {
        return discord_message("The Valorant rank worker is currently disabled.", true);
    };
    let Some(player) = service.player_for_discord_user(discord_user_id) else {
        return discord_message(
            "Your Discord account is not linked to a Valorant Riot ID in `config/league-valorant-rank.ron`.",
            true,
        );
    };
    if let Some(remaining) = claim_manual_poll(&state.cooldowns, Command::Valorant, discord_user_id)
    {
        return cooldown_message(Command::Valorant, remaining);
    }
    let Some(token) = interaction.token else {
        return discord_message("Discord did not include an interaction token.", true);
    };

    let service = Arc::clone(service);
    let discord_client = Arc::clone(&state.discord_client);
    let application_id = state.application_id;
    tokio::spawn(async move {
        let (content, allowed_mentions) = match service.prepare_report(&player, false).await {
            Ok(report) => {
                info!(riot_id = %player.riot_id, "Prepared manual Valorant rank report");
                (report.message, report.allowed_mentions)
            }
            Err(err) => {
                error!(riot_id = %player.riot_id, "Manual Valorant rank report failed: {err}");
                (
                    "I couldn't load your Valorant rank right now. Please try again shortly."
                        .to_string(),
                    AllowedMentions::default(),
                )
            }
        };
        if let Err(err) = discord_client
            .interaction(application_id)
            .update_response(&token)
            .content(Some(&content))
            .allowed_mentions(Some(&allowed_mentions))
            .await
        {
            error!(riot_id = %player.riot_id, "Failed to finish /valorant response: {err}");
        }
    });
    discord_response(DEFERRED_MESSAGE_RESPONSE, None)
}

fn cooldown_message(command: Command, remaining: u64) -> Response {
    discord_message(
        &format!(
            "Please wait {remaining} seconds before using `/{}` again.",
            command.name()
        ),
        true,
    )
}

fn claim_manual_poll(
    cooldowns: &Mutex<HashMap<(Command, Id<UserMarker>), Instant>>,
    command: Command,
    user_id: Id<UserMarker>,
) -> Option<u64> {
    let mut cooldowns = cooldowns.lock().unwrap_or_else(|err| err.into_inner());
    let now = Instant::now();
    let key = (command, user_id);
    if let Some(last_poll) = cooldowns.get(&key) {
        let elapsed = now.saturating_duration_since(*last_poll);
        if elapsed < MANUAL_POLL_COOLDOWN {
            return Some((MANUAL_POLL_COOLDOWN - elapsed).as_secs().max(1));
        }
    }
    cooldowns.insert(key, now);
    None
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn verify_request(
    public_key: &VerifyingKey,
    signature_hex: &str,
    timestamp: &str,
    body: &[u8],
) -> bool {
    let Ok(timestamp_seconds) = timestamp.parse::<u64>() else {
        return false;
    };
    let Ok(now_seconds) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    if now_seconds.abs_diff(Duration::from_secs(timestamp_seconds)) > SIGNATURE_MAX_AGE {
        return false;
    }
    let Ok(signature_bytes) = hex::decode(signature_hex) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature_bytes) else {
        return false;
    };
    let mut message = Vec::with_capacity(timestamp.len() + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.extend_from_slice(body);
    public_key.verify_strict(&message, &signature).is_ok()
}

fn discord_message(content: &str, ephemeral: bool) -> Response {
    let mut data = json!({
        "content": content,
        "allowed_mentions": { "parse": [] },
    });
    if ephemeral {
        data["flags"] = json!(EPHEMERAL_FLAG);
    }
    discord_response(MESSAGE_RESPONSE, Some(data))
}

fn discord_response(kind: u8, data: Option<serde_json::Value>) -> Response {
    let payload = match data {
        Some(data) => json!({ "type": kind, "data": data }),
        None => json!({ "type": kind }),
    };
    axum::Json(payload).into_response()
}

/// Read only the guild of the already configured bot channel; never print credentials.
pub(crate) async fn registration_info() -> Result<()> {
    let application_id = parse_application_id(&require_env("DISCORD_APPLICATION_ID")?)?;
    let channel_id = parse_id::<twilight_model::id::marker::ChannelMarker>(
        &require_env("DISCORD_CHANNEL_ID")?,
        "DISCORD_CHANNEL_ID",
    )?;
    let response = HttpClient::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()?
        .get(format!(
            "https://discord.com/api/v10/channels/{}",
            channel_id.get()
        ))
        .header(
            "Authorization",
            format!("Bot {}", require_env("DISCORD_BOT_TOKEN")?),
        )
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(Error::Config(format!(
            "Could not read configured Discord channel ({})",
            response.status()
        )));
    }
    let channel: serde_json::Value = response.json().await?;
    let guild_id = channel
        .get("guild_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Config("Configured Discord channel has no server ID".into()))?;
    let guild_id = parse_guild_id(guild_id)?;
    println!(
        "{}",
        json!({"applicationId": application_id.get().to_string(), "guildId": guild_id.get().to_string()})
    );
    Ok(())
}

fn register_on_startup(value: Option<&str>) -> Result<bool> {
    match value.map(str::to_ascii_lowercase).as_deref() {
        None | Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        _ => Err(Error::Config(
            "DISCORD_REGISTER_COMMANDS_ON_STARTUP must be true or false".into(),
        )),
    }
}

async fn register_commands(
    bot_token: &str,
    config: &InteractionConfig,
    commands: &[Command],
) -> Result<()> {
    let client = HttpClient::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("chunguschillercord/0.1 Discord command registration")
        .build()?;
    for command in commands {
        let (url, payload) = command_registration(config, *command);
        let response = client
            .post(url)
            .header("Authorization", format!("Bot {bot_token}"))
            .json(&payload)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(Error::Config(format!(
                "Discord failed to register /{} ({status}): {}",
                command.name(),
                detail.chars().take(500).collect::<String>()
            )));
        }
        info!(
            command = command.name(),
            scope = if config.guild_id.is_some() {
                "guild"
            } else {
                "global"
            },
            "Registered Discord command"
        );
    }
    Ok(())
}

fn command_registration(
    config: &InteractionConfig,
    command: Command,
) -> (String, serde_json::Value) {
    let mut payload = json!({
        "name": command.name(),
        "description": command.description(),
        "type": 1,
    });
    let url = match config.guild_id {
        Some(guild_id) => format!(
            "https://discord.com/api/v10/applications/{}/guilds/{}/commands",
            config.application_id.get(),
            guild_id.get()
        ),
        None => {
            payload["contexts"] = json!([0]);
            payload["integration_types"] = json!([0]);
            format!(
                "https://discord.com/api/v10/applications/{}/commands",
                config.application_id.get()
            )
        }
    };
    (url, payload)
}

fn parse_application_id(value: &str) -> Result<Id<ApplicationMarker>> {
    parse_id(value, "DISCORD_APPLICATION_ID")
}

fn parse_guild_id(value: &str) -> Result<Id<GuildMarker>> {
    parse_id(value, "DISCORD_GUILD_ID")
}

fn parse_id<T>(value: &str, name: &str) -> Result<Id<T>> {
    let value = value
        .parse::<u64>()
        .map_err(|_| Error::Config(format!("{name} must be a positive integer")))?;
    Id::new_checked(value).ok_or_else(|| Error::Config(format!("{name} must be greater than zero")))
}

fn parse_public_key(value: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(value)
        .map_err(|_| Error::Config("DISCORD_PUBLIC_KEY must be hexadecimal".to_string()))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        Error::Config("DISCORD_PUBLIC_KEY must decode to exactly 32 bytes".to_string())
    })?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| Error::Config("DISCORD_PUBLIC_KEY is not a valid Ed25519 key".to_string()))
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn require_env(name: &str) -> Result<String> {
    env_value(name).ok_or_else(|| Error::Config(format!("Missing env var {name}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    #[test]
    fn verifies_signatures_over_timestamp_and_raw_body() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let body = br#"{"type":1}"#;
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body);
        let signature_hex = hex::encode(signing_key.sign(&message).to_bytes());

        assert!(verify_request(
            &signing_key.verifying_key(),
            &signature_hex,
            &timestamp,
            body,
        ));
        assert!(!verify_request(
            &signing_key.verifying_key(),
            &signature_hex,
            &timestamp,
            br#"{"type":2}"#,
        ));
    }

    #[test]
    fn cooldowns_are_per_user_and_per_command() {
        let cooldowns = Mutex::new(HashMap::new());
        let user = Id::new(123_456_789);

        assert_eq!(claim_manual_poll(&cooldowns, Command::League, user), None);
        assert!(claim_manual_poll(&cooldowns, Command::League, user).is_some());
        assert_eq!(claim_manual_poll(&cooldowns, Command::Valorant, user), None);
        assert_eq!(claim_manual_poll(&cooldowns, Command::Keys, user), None);
        assert!(claim_manual_poll(&cooldowns, Command::Keys, user).is_some());
        assert_eq!(
            claim_manual_poll(&cooldowns, Command::League, Id::new(987_654_321)),
            None
        );
    }

    #[test]
    fn registers_commands_in_existing_guild_or_global_scope() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let mut config = InteractionConfig {
            application_id: Id::new(123),
            public_key: signing_key.verifying_key(),
            guild_id: Some(Id::new(456)),
        };
        for command in [Command::League, Command::Valorant, Command::Keys] {
            let (url, payload) = command_registration(&config, command);
            assert!(url.ends_with("/applications/123/guilds/456/commands"));
            assert_eq!(payload["name"], command.name());
            assert!(payload.get("contexts").is_none());
        }

        config.guild_id = None;
        let (url, payload) = command_registration(&config, Command::Valorant);
        assert!(url.ends_with("/applications/123/commands"));
        assert_eq!(payload["contexts"], json!([0]));
    }
    #[test]
    fn keys_access_requires_member_of_configured_guild() {
        assert_eq!(Command::from_name("keys"), Some(Command::Keys));
        let mut interaction: DiscordInteraction = serde_json::from_value(json!({
            "type": 2, "data": {"name":"keys"}, "guild_id":"456",
            "member":{"user":{"id":"789"}}
        }))
        .unwrap();
        assert!(keys_guild_allowed(&interaction, Some(Id::new(456))));
        assert!(!keys_guild_allowed(&interaction, Some(Id::new(123))));
        assert!(!keys_guild_allowed(&interaction, None));
        interaction.guild_id = None;
        assert!(!keys_guild_allowed(&interaction, Some(Id::new(456))));
        interaction.guild_id = Some("456".into());
        interaction.member = None;
        assert!(!keys_guild_allowed(&interaction, Some(Id::new(456))));
    }

    #[tokio::test]
    async fn signed_keys_request_in_wrong_guild_is_ephemeral_without_discord_call() {
        crate::install_rustls_crypto_provider();
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let state = Arc::new(InteractionState {
            league: None,
            valorant: None,
            keys: None,
            guild_id: Some(Id::new(456)),
            discord_client: Arc::new(DiscordClient::new("local-fixture-unused".into())),
            application_id: Id::new(123),
            public_key: signing_key.verifying_key(),
            cooldowns: Mutex::new(HashMap::new()),
        });
        let body = Bytes::from_static(
            br#"{"type":2,"data":{"name":"keys"},"guild_id":"999","member":{"user":{"id":"789"}}}"#,
        );
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let mut signed = timestamp.as_bytes().to_vec();
        signed.extend_from_slice(&body);
        let mut headers = HeaderMap::new();
        headers.insert("X-Signature-Timestamp", timestamp.parse().unwrap());
        headers.insert(
            "X-Signature-Ed25519",
            hex::encode(signing_key.sign(&signed).to_bytes())
                .parse()
                .unwrap(),
        );
        let response = handle_interaction(State(state), headers, body).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["type"], MESSAGE_RESPONSE);
        assert_eq!(payload["data"]["flags"], EPHEMERAL_FLAG);
        assert_eq!(payload["data"]["allowed_mentions"]["parse"], json!([]));
        assert!(
            payload["data"]["content"]
                .as_str()
                .unwrap()
                .contains("configured server")
        );
    }
    #[test]
    fn automatic_registration_can_be_explicitly_disabled() {
        assert!(register_on_startup(None).unwrap());
        for value in ["0", "false", "FALSE", "off", "no"] {
            assert!(!register_on_startup(Some(value)).unwrap());
        }
        assert!(register_on_startup(Some("true")).unwrap());
        assert!(register_on_startup(Some("typo")).is_err());
    }
}
