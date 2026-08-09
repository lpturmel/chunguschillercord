use crate::error::{Error, Result};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode as HttpStatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::{DateTime, Days, LocalResult, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use ed25519_dalek::{Signature, VerifyingKey};
use reqwest::{Client as HttpClient, StatusCode, Url, header::RETRY_AFTER};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Mutex as AsyncMutex, time::sleep};
use tracing::{error, info, warn};
use twilight_http::Client as DiscordClient;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ApplicationMarker, ChannelMarker, GuildMarker, UserMarker},
    },
};

const DEFAULT_CONFIG_PATH: &str = "config/league-rank.ron";
const DEFAULT_DATABASE_PATH: &str = "/tmp/chunguschillercord.db";
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "create_schema_migrations",
        sql: include_str!("../migrations/0001_create_schema_migrations.sql"),
    },
    Migration {
        version: 2,
        name: "create_league_rank_snapshots",
        sql: include_str!("../migrations/0002_create_league_rank_snapshots.sql"),
    },
];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MATCH_REQUEST_PAUSE: Duration = Duration::from_millis(75);
const RANKED_SOLO_QUEUE_ID: u16 = 420;
const RANKED_SOLO_QUEUE_TYPE: &str = "RANKED_SOLO_5x5";
const MANUAL_POLL_COOLDOWN: Duration = Duration::from_secs(60);
const DISCORD_SIGNATURE_MAX_AGE: Duration = Duration::from_secs(5 * 60);
const DISCORD_EPHEMERAL_FLAG: u64 = 1 << 6;
const DISCORD_PING: u8 = 1;
const DISCORD_APPLICATION_COMMAND: u8 = 2;
const DISCORD_PONG_RESPONSE: u8 = 1;
const DISCORD_MESSAGE_RESPONSE: u8 = 4;
const DISCORD_DEFERRED_MESSAGE_RESPONSE: u8 = 5;

#[derive(Clone)]
struct Config {
    riot_api_key: String,
    discord_bot_token: String,
    discord_channel_id: Id<ChannelMarker>,
    time_zone: Tz,
    post_times: Vec<NaiveTime>,
    users: Vec<PlayerConfig>,
    post_on_startup: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlayerConfig {
    riot_id: String,
    game_name: String,
    tag_line: String,
    platform: String,
    regional_route: String,
    discord_user_id: Option<Id<UserMarker>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    timezone: String,
    post_times: Vec<String>,
    users: Vec<FilePlayerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FilePlayerConfig {
    riot_id: String,
    platform: String,
    regional_route: String,
    #[serde(default)]
    discord_user_id: Option<u64>,
}

impl Config {
    fn from_env() -> Result<Option<Self>> {
        let Some(riot_api_key) = env_value("RIOT_API_KEY") else {
            info!("League rank bot is disabled; RIOT_API_KEY is not set");
            return Ok(None);
        };

        let discord_bot_token = require_env("DISCORD_BOT_TOKEN")?;
        let discord_channel_id = parse_channel_id(&require_env("DISCORD_CHANNEL_ID")?)?;
        let config_path = env_value("LEAGUE_RANK_CONFIG").unwrap_or_else(default_config_path);
        let source = fs::read_to_string(&config_path).map_err(|err| {
            Error::Config(format!(
                "Failed to read League rank config {config_path}: {err}"
            ))
        })?;
        let file_config = ron::from_str::<FileConfig>(&source).map_err(|err| {
            Error::Config(format!(
                "Failed to parse League rank config {config_path}: {err}"
            ))
        })?;
        let (time_zone, post_times, users) = validate_file_config(file_config)?;

        Ok(Some(Self {
            riot_api_key,
            discord_bot_token,
            discord_channel_id,
            time_zone,
            post_times,
            users,
            post_on_startup: env_flag("LEAGUE_RANK_POST_ON_STARTUP"),
        }))
    }
}

pub async fn initialize_from_env() -> Result<Router> {
    let Some(config) = Config::from_env()? else {
        return Ok(Router::new());
    };
    let store = RankStore::from_env().await?;
    let riot_client = RiotClient::new(config.riot_api_key.clone())
        .map_err(|err| Error::Config(format!("Failed to initialize Riot API client: {err}")))?;
    let service = Arc::new(LeagueService {
        riot_client,
        store,
        time_zone: config.time_zone,
        users: Arc::from(config.users.clone()),
    });
    let interaction_config = InteractionConfig::from_env()?;
    let interaction_router = if let Some(interaction_config) = interaction_config {
        register_league_command(&config.discord_bot_token, &interaction_config).await?;
        let state = Arc::new(InteractionState {
            service: Arc::clone(&service),
            discord_client: Arc::new(DiscordClient::new(config.discord_bot_token.clone())),
            application_id: interaction_config.application_id,
            public_key: interaction_config.public_key,
            cooldowns: Mutex::new(HashMap::new()),
        });

        info!("Discord /league interaction endpoint is enabled");
        Router::new()
            .route("/discord/interactions", post(handle_discord_interaction))
            .with_state(state)
    } else {
        info!(
            "Discord /league command is disabled; DISCORD_APPLICATION_ID and DISCORD_PUBLIC_KEY are not set"
        );
        Router::new()
    };

    info!(
        users = config.users.len(),
        time_zone = %config.time_zone,
        post_times = %format_post_times(&config.post_times),
        "Starting League rank bot"
    );
    tokio::spawn(run(config, service));
    Ok(interaction_router)
}

async fn run(config: Config, service: Arc<LeagueService>) {
    let discord_client = DiscordClient::new(config.discord_bot_token.clone());

    if config.post_on_startup {
        info!("Posting immediate League rank recaps for integration testing");
        post_all_reports(&service, &discord_client, &config).await;
    }

    loop {
        let now = Utc::now();
        let next_post = next_post_after(now, config.time_zone, &config.post_times);
        let wait = (next_post - now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(0));

        info!(
            next_post = %next_post.with_timezone(&config.time_zone),
            "League rank recap scheduled"
        );
        sleep(wait).await;
        post_all_reports(&service, &discord_client, &config).await;
    }
}

async fn post_all_reports(
    service: &LeagueService,
    discord_client: &DiscordClient,
    config: &Config,
) {
    for player in &config.users {
        if let Err(err) = post_player_report(service, discord_client, config, player).await {
            error!(riot_id = %player.riot_id, "Failed to post League rank recap: {err}");
        }
    }
}

async fn post_player_report(
    service: &LeagueService,
    discord_client: &DiscordClient,
    config: &Config,
    player: &PlayerConfig,
) -> WorkerResult<()> {
    let report = service.prepare_report(player, true).await?;

    discord_client
        .create_message(config.discord_channel_id)
        .content(&report.message)
        .allowed_mentions(Some(&report.allowed_mentions))
        .await?;

    service.store.save_snapshot(&report.current).await?;
    info!(
        riot_id = %player.riot_id,
        rank = %report.current.rank.label(),
        lp = report.current.rank.league_points,
        wins = report.record.wins,
        losses = report.record.losses,
        "Posted League rank recap to Discord"
    );
    Ok(())
}

#[derive(Clone)]
struct LeagueService {
    riot_client: RiotClient,
    store: RankStore,
    time_zone: Tz,
    users: Arc<[PlayerConfig]>,
}

struct PreparedReport {
    message: String,
    allowed_mentions: AllowedMentions,
    current: RankSnapshot,
    record: MatchRecord,
}

impl LeagueService {
    async fn prepare_report(
        &self,
        player: &PlayerConfig,
        include_mention: bool,
    ) -> WorkerResult<PreparedReport> {
        let now = Utc::now();
        let account = self.riot_client.resolve_account(player).await?;
        let current_rank = self
            .riot_client
            .fetch_solo_rank(player, &account.puuid)
            .await?;
        let previous = self.store.latest_snapshot(player, &account.puuid).await?;
        let record = match &previous {
            Some(snapshot) => {
                self.riot_client
                    .fetch_ranked_record(player, &account.puuid, snapshot.captured_at, now)
                    .await?
            }
            None => MatchRecord::default(),
        };
        let current = RankSnapshot {
            riot_id: player.riot_id.clone(),
            puuid: account.puuid,
            platform: player.platform.clone(),
            captured_at: now,
            rank: current_rank,
        };

        Ok(PreparedReport {
            message: format_report(
                player,
                previous.as_ref(),
                &current,
                record,
                self.time_zone,
                include_mention,
            ),
            allowed_mentions: allowed_mentions_for_player(player, include_mention),
            current,
            record,
        })
    }

    fn player_for_discord_user(&self, discord_user_id: Id<UserMarker>) -> Option<PlayerConfig> {
        self.users
            .iter()
            .find(|player| player.discord_user_id == Some(discord_user_id))
            .cloned()
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

        let application_id = parse_application_id(
            application_id
                .as_deref()
                .ok_or_else(|| Error::Config("Missing env var DISCORD_APPLICATION_ID".into()))?,
        )?;
        let public_key = parse_discord_public_key(
            public_key
                .as_deref()
                .ok_or_else(|| Error::Config("Missing env var DISCORD_PUBLIC_KEY".into()))?,
        )?;
        let guild_id = guild_id.as_deref().map(parse_guild_id).transpose()?;

        Ok(Some(Self {
            application_id,
            public_key,
            guild_id,
        }))
    }
}

struct InteractionState {
    service: Arc<LeagueService>,
    discord_client: Arc<DiscordClient>,
    application_id: Id<ApplicationMarker>,
    public_key: VerifyingKey,
    cooldowns: Mutex<HashMap<Id<UserMarker>, Instant>>,
}

#[derive(Debug, Deserialize)]
struct DiscordInteraction {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    token: Option<String>,
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

async fn handle_discord_interaction(
    State(state): State<Arc<InteractionState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(signature) = header_text(&headers, "X-Signature-Ed25519") else {
        return (HttpStatusCode::UNAUTHORIZED, "missing Discord signature").into_response();
    };
    let Some(timestamp) = header_text(&headers, "X-Signature-Timestamp") else {
        return (HttpStatusCode::UNAUTHORIZED, "missing Discord timestamp").into_response();
    };
    if !verify_discord_request(&state.public_key, signature, timestamp, &body) {
        return (HttpStatusCode::UNAUTHORIZED, "invalid Discord signature").into_response();
    }

    let interaction = match serde_json::from_slice::<DiscordInteraction>(&body) {
        Ok(interaction) => interaction,
        Err(err) => {
            warn!("Discord interaction JSON was invalid: {err}");
            return (HttpStatusCode::BAD_REQUEST, "invalid Discord interaction").into_response();
        }
    };

    if interaction.kind == DISCORD_PING {
        return discord_response(DISCORD_PONG_RESPONSE, None);
    }
    if interaction.kind != DISCORD_APPLICATION_COMMAND
        || interaction.data.as_ref().map(|data| data.name.as_str()) != Some("league")
    {
        return discord_message("This endpoint only handles the `/league` command.", true);
    }

    let Some(discord_user_id) = interaction.author_id() else {
        return discord_message("Discord did not include the invoking user.", true);
    };
    let Some(player) = state.service.player_for_discord_user(discord_user_id) else {
        return discord_message(
            "Your Discord account is not linked to a Riot ID. Add your Discord user ID to `config/league-rank.ron` and restart the app.",
            true,
        );
    };
    if let Some(remaining) = claim_manual_poll(&state.cooldowns, discord_user_id) {
        return discord_message(
            &format!("Please wait {remaining} seconds before using `/league` again."),
            true,
        );
    }
    let Some(token) = interaction.token else {
        return discord_message("Discord did not include an interaction token.", true);
    };

    let service = Arc::clone(&state.service);
    let discord_client = state.discord_client.clone();
    let application_id = state.application_id;
    tokio::spawn(async move {
        let result = service.prepare_report(&player, false).await;
        let (content, allowed_mentions) = match result {
            Ok(report) => {
                info!(
                    riot_id = %player.riot_id,
                    "Prepared manual League rank recap without changing the scheduled snapshot"
                );
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

    discord_response(DISCORD_DEFERRED_MESSAGE_RESPONSE, None)
}

fn claim_manual_poll(
    cooldowns: &Mutex<HashMap<Id<UserMarker>, Instant>>,
    user_id: Id<UserMarker>,
) -> Option<u64> {
    let mut cooldowns = cooldowns.lock().unwrap_or_else(|err| err.into_inner());
    let now = Instant::now();
    if let Some(last_poll) = cooldowns.get(&user_id) {
        let elapsed = now.saturating_duration_since(*last_poll);
        if elapsed < MANUAL_POLL_COOLDOWN {
            return Some((MANUAL_POLL_COOLDOWN - elapsed).as_secs().max(1));
        }
    }
    cooldowns.insert(user_id, now);
    None
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn verify_discord_request(
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
    if now_seconds.abs_diff(Duration::from_secs(timestamp_seconds)) > DISCORD_SIGNATURE_MAX_AGE {
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
        data["flags"] = json!(DISCORD_EPHEMERAL_FLAG);
    }
    discord_response(DISCORD_MESSAGE_RESPONSE, Some(data))
}

fn discord_response(kind: u8, data: Option<serde_json::Value>) -> Response {
    let payload = match data {
        Some(data) => json!({ "type": kind, "data": data }),
        None => json!({ "type": kind }),
    };
    axum::Json(payload).into_response()
}

async fn register_league_command(bot_token: &str, config: &InteractionConfig) -> Result<()> {
    let (url, command) = command_registration(config);
    let response = HttpClient::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("chunguschillercord/0.1 Discord command registration")
        .build()?
        .post(url)
        .header("Authorization", format!("Bot {bot_token}"))
        .json(&command)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().await.unwrap_or_default();
        return Err(Error::Config(format!(
            "Discord failed to register /league ({status}): {}",
            detail.chars().take(500).collect::<String>()
        )));
    }

    info!(
        scope = if config.guild_id.is_some() {
            "guild"
        } else {
            "global"
        },
        "Registered Discord /league command"
    );
    Ok(())
}

fn command_registration(config: &InteractionConfig) -> (String, serde_json::Value) {
    match config.guild_id {
        Some(guild_id) => (
            format!(
                "https://discord.com/api/v10/applications/{}/guilds/{}/commands",
                config.application_id.get(),
                guild_id.get()
            ),
            json!({
                "name": "league",
                "description": "Show your League ranked recap",
                "type": 1,
            }),
        ),
        None => (
            format!(
                "https://discord.com/api/v10/applications/{}/commands",
                config.application_id.get()
            ),
            json!({
                "name": "league",
                "description": "Show your League ranked recap",
                "type": 1,
                "contexts": [0],
                "integration_types": [0],
            }),
        ),
    }
}

fn parse_application_id(value: &str) -> Result<Id<ApplicationMarker>> {
    let value = value.parse::<u64>().map_err(|_| {
        Error::Config("DISCORD_APPLICATION_ID must be a positive integer".to_string())
    })?;
    Id::new_checked(value).ok_or_else(|| {
        Error::Config("DISCORD_APPLICATION_ID must be greater than zero".to_string())
    })
}

fn parse_guild_id(value: &str) -> Result<Id<GuildMarker>> {
    let value = value
        .parse::<u64>()
        .map_err(|_| Error::Config("DISCORD_GUILD_ID must be a positive integer".to_string()))?;
    Id::new_checked(value)
        .ok_or_else(|| Error::Config("DISCORD_GUILD_ID must be greater than zero".to_string()))
}

fn parse_discord_public_key(value: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(value)
        .map_err(|_| Error::Config("DISCORD_PUBLIC_KEY must be hexadecimal".to_string()))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        Error::Config("DISCORD_PUBLIC_KEY must decode to exactly 32 bytes".to_string())
    })?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| Error::Config("DISCORD_PUBLIC_KEY is not a valid Ed25519 key".to_string()))
}

#[derive(Clone)]
struct RiotClient {
    http: HttpClient,
    api_key: String,
}

impl RiotClient {
    fn new(api_key: String) -> WorkerResult<Self> {
        let http = HttpClient::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("chunguschillercord/0.1 League rank bot")
            .build()?;
        Ok(Self { http, api_key })
    }

    async fn resolve_account(&self, player: &PlayerConfig) -> WorkerResult<AccountDto> {
        let url = riot_url(
            &player.regional_route,
            &[
                "riot",
                "account",
                "v1",
                "accounts",
                "by-riot-id",
                &player.game_name,
                &player.tag_line,
            ],
        );
        self.get_json(url).await
    }

    async fn fetch_solo_rank(&self, player: &PlayerConfig, puuid: &str) -> WorkerResult<Rank> {
        let url = riot_url(
            &player.platform,
            &["lol", "league", "v4", "entries", "by-puuid", puuid],
        );
        let entries = self.get_json::<Vec<LeagueEntryDto>>(url).await?;

        Ok(entries
            .into_iter()
            .find(|entry| entry.queue_type == RANKED_SOLO_QUEUE_TYPE)
            .map(Rank::from)
            .unwrap_or_else(Rank::unranked))
    }

    async fn fetch_ranked_record(
        &self,
        player: &PlayerConfig,
        puuid: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> WorkerResult<MatchRecord> {
        if start >= end {
            return Ok(MatchRecord::default());
        }

        let mut url = riot_url(
            &player.regional_route,
            &["lol", "match", "v5", "matches", "by-puuid", puuid, "ids"],
        );
        url.query_pairs_mut()
            .append_pair("queue", &RANKED_SOLO_QUEUE_ID.to_string())
            .append_pair("startTime", &start.timestamp().to_string())
            .append_pair("endTime", &end.timestamp().to_string())
            .append_pair("start", "0")
            .append_pair("count", "100");
        let match_ids = self.get_json::<Vec<String>>(url).await?;
        let mut record = MatchRecord::default();

        for match_id in match_ids {
            sleep(MATCH_REQUEST_PAUSE).await;
            let url = riot_url(
                &player.regional_route,
                &["lol", "match", "v5", "matches", &match_id],
            );
            let match_data = self.get_json::<MatchDto>(url).await?;
            let participant = match_data
                .info
                .participants
                .into_iter()
                .find(|participant| participant.puuid == puuid)
                .ok_or_else(|| {
                    WorkerError::Data(format!(
                        "Riot match {match_id} did not contain configured player {}",
                        player.riot_id
                    ))
                })?;

            if participant.win {
                record.wins += 1;
            } else {
                record.losses += 1;
            }
        }

        Ok(record)
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> WorkerResult<T> {
        for attempt in 0..=1 {
            let response = self
                .http
                .get(url.clone())
                .header("X-Riot-Token", &self.api_key)
                .send()
                .await?;
            let status = response.status();

            if status == StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                let retry_after = response
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(2)
                    .min(120);
                warn!(retry_after, "Riot API rate limit reached; retrying request");
                sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(WorkerError::RiotApi {
                    status,
                    detail: body.chars().take(500).collect(),
                });
            }

            return Ok(response.json::<T>().await?);
        }

        unreachable!("Riot API request either returns or retries once")
    }
}

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

#[derive(Clone)]
enum BotDatabase {
    #[cfg(test)]
    Local(turso::Database),
    Synced(turso::sync::Database),
}

impl BotDatabase {
    async fn connect(&self) -> turso::Result<turso::Connection> {
        match self {
            #[cfg(test)]
            Self::Local(database) => database.connect(),
            Self::Synced(database) => database.connect().await,
        }
    }

    async fn pull(&self) -> turso::Result<()> {
        match self {
            Self::Synced(database) => {
                database.pull().await?;
            }
            #[cfg(test)]
            Self::Local(_) => {}
        }
        Ok(())
    }

    async fn push(&self) -> turso::Result<()> {
        match self {
            Self::Synced(database) => {
                database.push().await?;
            }
            #[cfg(test)]
            Self::Local(_) => {}
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RankStore {
    db: BotDatabase,
    sync_lock: Arc<AsyncMutex<()>>,
}

impl RankStore {
    async fn from_env() -> Result<Self> {
        let database_url = require_env("TURSO_DATABASE_URL")?;
        let auth_token = require_env("TURSO_AUTH_TOKEN")?;
        let database_path = env_value("CHUNGUSCHILLERCORD_DATABASE_PATH")
            .unwrap_or_else(|| DEFAULT_DATABASE_PATH.to_string());
        if let Some(parent) = std::path::Path::new(&database_path).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        info!(%database_path, "Opening Turso synced database");
        let database = turso::sync::Builder::new_remote(&database_path)
            .with_remote_url(database_url)
            .with_auth_token(auth_token)
            .with_client_name("chunguschillercord")
            .build()
            .await?;
        let store = Self::new(BotDatabase::Synced(database)).await?;
        info!(%database_path, "Turso database is ready");
        Ok(store)
    }

    #[cfg(test)]
    async fn new_local(path: &std::path::Path) -> Result<Self> {
        let path = path
            .to_str()
            .ok_or_else(|| Error::Config("Test database path is not valid UTF-8".to_string()))?;
        let database = turso::Builder::new_local(path).build().await?;
        Self::new(BotDatabase::Local(database)).await
    }

    async fn new(db: BotDatabase) -> Result<Self> {
        let store = Self {
            db,
            sync_lock: Arc::new(AsyncMutex::new(())),
        };
        store.run_migrations().await?;
        Ok(store)
    }

    async fn run_migrations(&self) -> Result<()> {
        let _guard = self.sync_lock.lock().await;
        self.db.pull().await?;
        let connection = self.db.connect().await?;

        connection.execute_batch(MIGRATIONS[0].sql).await?;
        for migration in MIGRATIONS {
            let mut rows = connection
                .query(
                    "SELECT 1 FROM chunguschillercord_schema_migrations WHERE version = ? LIMIT 1",
                    (migration.version,),
                )
                .await?;
            if rows.next().await?.is_some() {
                continue;
            }
            if migration.version != MIGRATIONS[0].version {
                connection.execute_batch(migration.sql).await?;
            }
            connection
                .execute(
                    "INSERT INTO chunguschillercord_schema_migrations
                     (version, name, applied_at) VALUES (?, ?, ?)",
                    (migration.version, migration.name, Utc::now().to_rfc3339()),
                )
                .await?;
            info!(
                version = migration.version,
                name = migration.name,
                "Applied bot database migration"
            );
        }
        self.db.push().await?;
        Ok(())
    }

    async fn latest_snapshot(
        &self,
        player: &PlayerConfig,
        puuid: &str,
    ) -> WorkerResult<Option<RankSnapshot>> {
        let _guard = self.sync_lock.lock().await;
        self.db.pull().await?;
        let connection = self.db.connect().await?;
        let mut rows = connection
            .query(
                "SELECT riot_id, puuid, platform_route, captured_at, tier, division, league_points
                 FROM league_rank_snapshots
                 WHERE platform_route = ? AND puuid = ?
                 ORDER BY captured_at DESC, id DESC
                 LIMIT 1",
                (player.platform.as_str(), puuid),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let captured_at = DateTime::parse_from_rfc3339(&row.get::<String>(3)?)?.with_timezone(&Utc);
        let division = row.get::<String>(5)?;

        Ok(Some(RankSnapshot {
            riot_id: row.get::<String>(0)?,
            puuid: row.get::<String>(1)?,
            platform: row.get::<String>(2)?,
            captured_at,
            rank: Rank {
                tier: row.get::<String>(4)?,
                division: (!division.is_empty()).then_some(division),
                league_points: row.get::<i64>(6)? as i32,
            },
        }))
    }

    async fn save_snapshot(&self, snapshot: &RankSnapshot) -> WorkerResult<()> {
        let _guard = self.sync_lock.lock().await;
        let connection = self.db.connect().await?;
        connection
            .execute(
                "INSERT INTO league_rank_snapshots
             (riot_id, puuid, platform_route, captured_at, tier, division, league_points)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
                turso::params![
                    snapshot.riot_id.clone(),
                    snapshot.puuid.clone(),
                    snapshot.platform.clone(),
                    snapshot.captured_at.to_rfc3339(),
                    snapshot.rank.tier.clone(),
                    snapshot.rank.division.clone().unwrap_or_default(),
                    snapshot.rank.league_points as i64,
                ],
            )
            .await?;
        self.db.push().await?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RankSnapshot {
    riot_id: String,
    puuid: String,
    platform: String,
    captured_at: DateTime<Utc>,
    rank: Rank,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rank {
    tier: String,
    division: Option<String>,
    league_points: i32,
}

impl Rank {
    fn unranked() -> Self {
        Self {
            tier: "UNRANKED".to_string(),
            division: None,
            league_points: 0,
        }
    }

    fn label(&self) -> String {
        let tier = title_case_tier(&self.tier);
        match &self.division {
            Some(division) if !division.is_empty() => format!("{tier} {division}"),
            _ => tier,
        }
    }

    fn score(&self) -> Option<i32> {
        let tier_base = match self.tier.as_str() {
            "IRON" => 0,
            "BRONZE" => 400,
            "SILVER" => 800,
            "GOLD" => 1_200,
            "PLATINUM" => 1_600,
            "EMERALD" => 2_000,
            "DIAMOND" => 2_400,
            "MASTER" | "GRANDMASTER" | "CHALLENGER" => 2_800,
            _ => return None,
        };
        let division = match self.division.as_deref() {
            Some("IV") => 0,
            Some("III") => 100,
            Some("II") => 200,
            Some("I") => 300,
            None => 0,
            _ => return None,
        };
        Some(tier_base + division + self.league_points)
    }
}

impl From<LeagueEntryDto> for Rank {
    fn from(entry: LeagueEntryDto) -> Self {
        let division = if matches!(entry.tier.as_str(), "MASTER" | "GRANDMASTER" | "CHALLENGER") {
            None
        } else {
            Some(entry.rank)
        };
        Self {
            tier: entry.tier,
            division,
            league_points: entry.league_points,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MatchRecord {
    wins: u32,
    losses: u32,
}

impl MatchRecord {
    fn win_rate(self) -> Option<f64> {
        let games = self.wins + self.losses;
        (games > 0).then(|| self.wins as f64 / games as f64 * 100.0)
    }
}

fn format_report(
    player: &PlayerConfig,
    previous: Option<&RankSnapshot>,
    current: &RankSnapshot,
    record: MatchRecord,
    time_zone: Tz,
    include_mention: bool,
) -> String {
    let local_now = current.captured_at.with_timezone(&time_zone);
    let date = local_now.format("%A, %B %-d, %Y");
    let updated_at = local_now.format("%A, %B %-d, %Y at %H:%M:%S %Z");
    let current_label = current.rank.label();
    let mention = include_mention
        .then_some(player.discord_user_id)
        .flatten()
        .map(|user_id| format!("\n<@{}>", user_id.get()))
        .unwrap_or_default();
    let record_line = match record.win_rate() {
        Some(win_rate) => format!(
            "**Record:** **{}W / {}L** · **{win_rate:.2}% win rate**",
            record.wins, record.losses
        ),
        None if previous.is_some() => "**Record:** **0W / 0L** · no ranked solo games".to_string(),
        None => "**Record:** baseline created · no previous snapshot".to_string(),
    };
    let (start_line, net_line, interval_line) = match previous {
        Some(previous) => {
            let delta = lp_delta(&previous.rank, &current.rank);
            let net = delta
                .map(|value| format!("**{} LP**", signed_number(value)))
                .unwrap_or_else(|| "unavailable across unranked state".to_string());
            let start = format!(
                "> **Start:** {} · {} LP",
                previous.rank.label(),
                previous.rank.league_points
            );
            let interval_start = previous
                .captured_at
                .with_timezone(&time_zone)
                .format("%b %-d at %H:%M %Z");
            (
                start,
                format!("> **Net:** {net}"),
                format!("-# Interval began {interval_start}"),
            )
        }
        None => (
            "> **Start:** unavailable (first snapshot)".to_string(),
            "> **Net:** baseline created".to_string(),
            "-# The next scheduled post will include a complete LP recap.".to_string(),
        ),
    };

    format!(
        "## 🏆 League Ranked Recap\n\
         ### {}{mention}\n\
         **Current:** **{}** · **{} LP**\n\
         \n\
         **Recap for {date}**\n\
         {record_line}\n\
         \n\
         **LP recap**\n\
         {start_line}\n\
         > **End:** {current_label} · {} LP\n\
         {net_line}\n\
         \n\
         -# Updated {updated_at}\n\
         {interval_line}",
        escape_discord_markdown(&player.riot_id),
        current_label,
        current.rank.league_points,
        current.rank.league_points,
    )
}

fn allowed_mentions_for_player(player: &PlayerConfig, include_mention: bool) -> AllowedMentions {
    AllowedMentions {
        users: if include_mention {
            player.discord_user_id.iter().copied().collect()
        } else {
            Vec::new()
        },
        ..AllowedMentions::default()
    }
}

fn lp_delta(start: &Rank, end: &Rank) -> Option<i32> {
    Some(end.score()? - start.score()?)
}

fn signed_number(value: i32) -> String {
    if value > 0 {
        format!("+{value}")
    } else {
        value.to_string()
    }
}

fn title_case_tier(tier: &str) -> String {
    let mut characters = tier.chars();
    match characters.next() {
        Some(first) => format!(
            "{}{}",
            first.to_uppercase(),
            characters.as_str().to_ascii_lowercase()
        ),
        None => String::new(),
    }
}

fn escape_discord_markdown(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('~', "\\~")
        .replace('`', "\\`")
        .replace('|', "\\|")
        .replace('>', "\\>")
}

fn next_post_after(now: DateTime<Utc>, time_zone: Tz, post_times: &[NaiveTime]) -> DateTime<Utc> {
    let local_now = now.with_timezone(&time_zone);

    for day_offset in 0..=2 {
        let date = local_now
            .date_naive()
            .checked_add_days(Days::new(day_offset))
            .expect("next League rank post date is representable");

        for post_time in post_times {
            let local_time = date.and_time(*post_time);
            let candidate = match time_zone.from_local_datetime(&local_time) {
                LocalResult::Single(candidate) => candidate,
                LocalResult::Ambiguous(earlier, later) => {
                    if earlier > local_now {
                        earlier
                    } else {
                        later
                    }
                }
                LocalResult::None => continue,
            };

            if candidate > local_now {
                return candidate.with_timezone(&Utc);
            }
        }
    }

    unreachable!("a scheduled League rank post exists within the next two days")
}

fn validate_file_config(file: FileConfig) -> Result<(Tz, Vec<NaiveTime>, Vec<PlayerConfig>)> {
    let time_zone = file.timezone.parse::<Tz>().map_err(|_| {
        Error::Config(format!(
            "League rank timezone must be an IANA time zone; got {}",
            file.timezone
        ))
    })?;
    let mut post_times = file
        .post_times
        .iter()
        .map(|value| {
            NaiveTime::parse_from_str(value, "%H:%M").map_err(|_| {
                Error::Config(format!(
                    "League rank post time must use 24-hour HH:MM format; got {value}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    post_times.sort_unstable();
    post_times.dedup();
    if post_times.is_empty() {
        return Err(Error::Config(
            "League rank config must contain at least one post time".to_string(),
        ));
    }
    if file.users.is_empty() {
        return Err(Error::Config(
            "League rank config must contain at least one user".to_string(),
        ));
    }

    let mut seen = HashSet::new();
    let mut seen_discord_users = HashSet::new();
    let mut users = Vec::with_capacity(file.users.len());
    for raw in file.users {
        let (game_name, tag_line) = parse_riot_id(&raw.riot_id)?;
        let platform = raw.platform.trim().to_ascii_lowercase();
        if !is_platform_route(&platform) {
            return Err(Error::Config(format!(
                "Unsupported League platform route {} for {}",
                raw.platform, raw.riot_id
            )));
        }
        let regional_route = raw.regional_route.trim().to_ascii_lowercase();
        if !matches!(
            regional_route.as_str(),
            "americas" | "asia" | "europe" | "sea"
        ) {
            return Err(Error::Config(format!(
                "Unsupported Riot regional route {} for {}",
                raw.regional_route, raw.riot_id
            )));
        }
        let riot_id = format!("{game_name}#{tag_line}");
        let identity_key = format!("{platform}:{}", riot_id.to_lowercase());
        if !seen.insert(identity_key) {
            return Err(Error::Config(format!(
                "Duplicate League rank user {riot_id} on {platform}"
            )));
        }
        let discord_user_id = match raw.discord_user_id {
            Some(value) => Some(Id::new_checked(value).ok_or_else(|| {
                Error::Config(format!(
                    "Discord user ID for {riot_id} must be greater than zero"
                ))
            })?),
            None => None,
        };
        if let Some(discord_user_id) = discord_user_id
            && !seen_discord_users.insert(discord_user_id)
        {
            return Err(Error::Config(format!(
                "Discord user ID {} is linked to more than one League user",
                discord_user_id.get()
            )));
        }

        users.push(PlayerConfig {
            riot_id,
            game_name,
            tag_line,
            platform,
            regional_route,
            discord_user_id,
        });
    }

    Ok((time_zone, post_times, users))
}

fn parse_riot_id(value: &str) -> Result<(String, String)> {
    let value = value.trim();
    let Some((game_name, tag_line)) = value.rsplit_once('#') else {
        return Err(Error::Config(format!(
            "League Riot ID must use GameName#TagLine format; got {value}"
        )));
    };
    let game_name = game_name.trim();
    let tag_line = tag_line.trim();
    if game_name.is_empty() || tag_line.is_empty() {
        return Err(Error::Config(format!(
            "League Riot ID must include both a game name and tag line; got {value}"
        )));
    }
    Ok((game_name.to_string(), tag_line.to_string()))
}

fn is_platform_route(value: &str) -> bool {
    matches!(
        value,
        "br1"
            | "eun1"
            | "euw1"
            | "jp1"
            | "kr"
            | "la1"
            | "la2"
            | "me1"
            | "na1"
            | "oc1"
            | "ph2"
            | "ru"
            | "sg2"
            | "th2"
            | "tr1"
            | "tw2"
            | "vn2"
    )
}

fn riot_url(route: &str, path_segments: &[&str]) -> Url {
    let mut url = Url::parse(&format!("https://{route}.api.riotgames.com/"))
        .expect("validated Riot route produces a valid URL");
    url.path_segments_mut()
        .expect("Riot API URL supports path segments")
        .pop_if_empty()
        .extend(path_segments);
    url
}

fn format_post_times(post_times: &[NaiveTime]) -> String {
    post_times
        .iter()
        .map(|time| time.format("%H:%M").to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_channel_id(value: &str) -> Result<Id<ChannelMarker>> {
    let value = value
        .parse::<u64>()
        .map_err(|_| Error::Config("DISCORD_CHANNEL_ID must be a positive integer".to_string()))?;
    Id::new_checked(value)
        .ok_or_else(|| Error::Config("DISCORD_CHANNEL_ID must be greater than zero".to_string()))
}

fn default_config_path() -> String {
    let runtime_path = std::path::Path::new(DEFAULT_CONFIG_PATH);
    if runtime_path.exists() {
        return runtime_path.display().to_string();
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(DEFAULT_CONFIG_PATH)
        .display()
        .to_string()
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

fn env_flag(name: &str) -> bool {
    env_value(name)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[derive(Debug, Deserialize)]
struct AccountDto {
    puuid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeagueEntryDto {
    queue_type: String,
    tier: String,
    rank: String,
    league_points: i32,
}

#[derive(Debug, Deserialize)]
struct MatchDto {
    info: MatchInfoDto,
}

#[derive(Debug, Deserialize)]
struct MatchInfoDto {
    participants: Vec<ParticipantDto>,
}

#[derive(Debug, Deserialize)]
struct ParticipantDto {
    puuid: String,
    win: bool,
}

type WorkerResult<T> = std::result::Result<T, WorkerError>;

#[derive(Debug)]
enum WorkerError {
    Request(reqwest::Error),
    RiotApi { status: StatusCode, detail: String },
    Discord(twilight_http::Error),
    Database(turso::Error),
    Timestamp(chrono::ParseError),
    Data(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(err) => write!(formatter, "Riot API request failed: {err}"),
            Self::RiotApi { status, detail } => {
                write!(formatter, "Riot API returned {status}: {detail}")
            }
            Self::Discord(err) => write!(formatter, "Discord request failed: {err}"),
            Self::Database(err) => write!(formatter, "League snapshot database failed: {err}"),
            Self::Timestamp(err) => write!(formatter, "Invalid stored snapshot timestamp: {err}"),
            Self::Data(err) => formatter.write_str(err),
        }
    }
}

impl From<reqwest::Error> for WorkerError {
    fn from(err: reqwest::Error) -> Self {
        Self::Request(err)
    }
}

impl From<twilight_http::Error> for WorkerError {
    fn from(err: twilight_http::Error) -> Self {
        Self::Discord(err)
    }
}

impl From<turso::Error> for WorkerError {
    fn from(err: turso::Error) -> Self {
        Self::Database(err)
    }
}

impl From<chrono::ParseError> for WorkerError {
    fn from(err: chrono::ParseError) -> Self {
        Self::Timestamp(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    fn player() -> PlayerConfig {
        PlayerConfig {
            riot_id: "liights#6957".to_string(),
            game_name: "liights".to_string(),
            tag_line: "6957".to_string(),
            platform: "na1".to_string(),
            regional_route: "americas".to_string(),
            discord_user_id: None,
        }
    }

    fn rank(tier: &str, division: Option<&str>, league_points: i32) -> Rank {
        Rank {
            tier: tier.to_string(),
            division: division.map(str::to_string),
            league_points,
        }
    }

    #[test]
    fn parses_and_validates_ron_config() {
        let source = r#"(
            timezone: "America/Toronto",
            post_times: ["23:00", "09:00"],
            users: [(
                riot_id: "liights#6957",
                platform: "NA1",
                regional_route: "americas",
            )],
        )"#;
        let file: FileConfig = ron::from_str(source).unwrap();
        let (time_zone, times, users) = validate_file_config(file).unwrap();

        assert_eq!(time_zone, chrono_tz::America::Toronto);
        assert_eq!(format_post_times(&times), "09:00, 23:00");
        assert_eq!(users, vec![player()]);
    }

    #[test]
    fn rejects_malformed_riot_ids() {
        assert!(parse_riot_id("liights6957").is_err());
        assert!(parse_riot_id("#6957").is_err());
        assert!(parse_riot_id("liights#").is_err());
    }

    #[test]
    fn rejects_a_discord_user_linked_to_multiple_riot_ids() {
        let source = r#"(
            timezone: "America/Toronto",
            post_times: ["09:00"],
            users: [
                (
                    riot_id: "liights#6957",
                    platform: "na1",
                    regional_route: "americas",
                    discord_user_id: Some(123456789),
                ),
                (
                    riot_id: "rems#6666",
                    platform: "na1",
                    regional_route: "americas",
                    discord_user_id: Some(123456789),
                ),
            ],
        )"#;
        let file: FileConfig = ron::from_str(source).unwrap();

        assert!(validate_file_config(file).is_err());
    }

    #[test]
    fn schedules_configured_times_with_daylight_saving() {
        let times = vec![
            NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            NaiveTime::from_hms_opt(23, 0, 0).unwrap(),
        ];

        assert_eq!(
            next_post_after(utc(2026, 8, 9, 12, 59), chrono_tz::America::Toronto, &times),
            utc(2026, 8, 9, 13, 0)
        );
        assert_eq!(
            next_post_after(
                utc(2026, 12, 9, 13, 59),
                chrono_tz::America::Toronto,
                &times
            ),
            utc(2026, 12, 9, 14, 0)
        );
    }

    #[test]
    fn calculates_lp_across_divisions_and_apex_tiers() {
        assert_eq!(
            lp_delta(&rank("DIAMOND", Some("I"), 90), &rank("MASTER", None, 10)),
            Some(20)
        );
        assert_eq!(
            lp_delta(&rank("MASTER", None, 500), &rank("GRANDMASTER", None, 500)),
            Some(0)
        );
        assert_eq!(
            lp_delta(&rank("MASTER", None, 10), &rank("MASTER", None, 114)),
            Some(104)
        );
    }

    #[test]
    fn formats_complete_markdown_recap() {
        let previous = RankSnapshot {
            riot_id: "liights#6957".to_string(),
            puuid: "puuid".to_string(),
            platform: "na1".to_string(),
            captured_at: utc(2023, 12, 28, 14, 0),
            rank: rank("MASTER", None, 10),
        };
        let current = RankSnapshot {
            captured_at: utc(2023, 12, 28, 21, 45),
            rank: rank("MASTER", None, 114),
            ..previous.clone()
        };
        let message = format_report(
            &player(),
            Some(&previous),
            &current,
            MatchRecord { wins: 5, losses: 1 },
            chrono_tz::America::Toronto,
            true,
        );

        assert!(message.contains("liights#6957"));
        assert!(message.contains("**Current:** **Master** · **114 LP**"));
        assert!(message.contains("**5W / 1L** · **83.33% win rate**"));
        assert!(message.contains("> **Start:** Master · 10 LP"));
        assert!(message.contains("> **End:** Master · 114 LP"));
        assert!(message.contains("> **Net:** **+104 LP**"));
        assert!(message.contains("Thursday, December 28, 2023"));
    }

    #[test]
    fn formats_and_allows_only_the_configured_user_mention() {
        let mut mentioned_player = player();
        mentioned_player.discord_user_id = Some(Id::new(123_456_789));
        let current = RankSnapshot {
            riot_id: mentioned_player.riot_id.clone(),
            puuid: "puuid".to_string(),
            platform: "na1".to_string(),
            captured_at: utc(2026, 8, 9, 13, 0),
            rank: rank("MASTER", None, 114),
        };
        let message = format_report(
            &mentioned_player,
            None,
            &current,
            MatchRecord::default(),
            chrono_tz::America::Toronto,
            true,
        );
        let allowed_mentions = allowed_mentions_for_player(&mentioned_player, true);

        assert!(message.contains("<@123456789>"));
        assert_eq!(allowed_mentions.users, vec![Id::new(123_456_789)]);
        assert!(allowed_mentions.roles.is_empty());
        assert!(allowed_mentions.parse.is_empty());
    }

    #[test]
    fn manual_report_does_not_mention_the_invoking_user() {
        let mut mentioned_player = player();
        mentioned_player.discord_user_id = Some(Id::new(123_456_789));
        let current = RankSnapshot {
            riot_id: mentioned_player.riot_id.clone(),
            puuid: "puuid".to_string(),
            platform: "na1".to_string(),
            captured_at: utc(2026, 8, 9, 13, 0),
            rank: rank("MASTER", None, 114),
        };
        let message = format_report(
            &mentioned_player,
            None,
            &current,
            MatchRecord::default(),
            chrono_tz::America::Toronto,
            false,
        );
        let allowed_mentions = allowed_mentions_for_player(&mentioned_player, false);

        assert!(!message.contains("<@123456789>"));
        assert!(allowed_mentions.users.is_empty());
    }

    #[test]
    fn verifies_discord_signatures_over_timestamp_and_raw_body() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let body = br#"{"type":1}"#;
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body);
        let signature = signing_key.sign(&message);
        let signature_hex = hex::encode(signature.to_bytes());

        assert!(verify_discord_request(
            &signing_key.verifying_key(),
            &signature_hex,
            &timestamp,
            body,
        ));
        assert!(!verify_discord_request(
            &signing_key.verifying_key(),
            &signature_hex,
            &timestamp,
            br#"{"type":2}"#,
        ));
    }

    #[test]
    fn manual_poll_has_a_per_user_cooldown() {
        let cooldowns = Mutex::new(HashMap::new());
        let user = Id::new(123_456_789);

        assert_eq!(claim_manual_poll(&cooldowns, user), None);
        assert!(claim_manual_poll(&cooldowns, user).is_some());
        assert_eq!(claim_manual_poll(&cooldowns, Id::new(987_654_321)), None);
    }

    #[test]
    fn registers_guild_or_global_league_command() {
        let signing_key = SigningKey::from_bytes(&[7; 32]);
        let mut config = InteractionConfig {
            application_id: Id::new(123),
            public_key: signing_key.verifying_key(),
            guild_id: Some(Id::new(456)),
        };
        let (guild_url, guild_command) = command_registration(&config);
        assert!(guild_url.ends_with("/applications/123/guilds/456/commands"));
        assert_eq!(guild_command["name"], "league");
        assert!(guild_command.get("contexts").is_none());

        config.guild_id = None;
        let (global_url, global_command) = command_registration(&config);
        assert!(global_url.ends_with("/applications/123/commands"));
        assert_eq!(global_command["contexts"], json!([0]));
    }

    #[test]
    fn parses_current_riot_api_shapes() {
        let entries: Vec<LeagueEntryDto> = serde_json::from_str(
            r#"[{
                "queueType": "RANKED_SOLO_5x5",
                "tier": "MASTER",
                "rank": "I",
                "leaguePoints": 114,
                "wins": 20,
                "losses": 10
            }]"#,
        )
        .unwrap();
        let match_data: MatchDto = serde_json::from_str(
            r#"{
                "info": {
                    "participants": [
                        {"puuid": "configured-player", "win": true}
                    ]
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            Rank::from(entries.into_iter().next().unwrap()).label(),
            "Master"
        );
        assert!(match_data.info.participants[0].win);
    }

    #[test]
    fn checked_in_config_is_valid_ron() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_PATH);
        let source = fs::read_to_string(path).unwrap();
        let file: FileConfig = ron::from_str(&source).unwrap();
        validate_file_config(file).unwrap();
    }

    #[tokio::test]
    async fn persists_and_loads_the_latest_snapshot() {
        let database_path = std::env::temp_dir().join(format!(
            "chunguschillercord-league-rank-test-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let store = RankStore::new_local(&database_path).await.unwrap();
        let first = RankSnapshot {
            riot_id: "liights#6957".to_string(),
            puuid: "puuid".to_string(),
            platform: "na1".to_string(),
            captured_at: utc(2026, 8, 9, 13, 0),
            rank: rank("MASTER", None, 10),
        };
        let second = RankSnapshot {
            captured_at: utc(2026, 8, 10, 3, 0),
            rank: rank("MASTER", None, 114),
            ..first.clone()
        };

        store.save_snapshot(&first).await.unwrap();
        store.save_snapshot(&second).await.unwrap();

        let loaded = store.latest_snapshot(&player(), "puuid").await.unwrap();
        assert_eq!(loaded, Some(second));

        drop(store);
        std::fs::remove_file(database_path).unwrap();
    }
}
