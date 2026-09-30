use crate::{
    config::{LeaguePlayerConfig as PlayerConfig, Schedule},
    database::BotDatabase,
    error::{Error, Result},
};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use reqwest::{Client as HttpClient, StatusCode, Url, header::RETRY_AFTER};
use serde::{Deserialize, de::DeserializeOwned};
use std::{fmt, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{error, info, warn};
use twilight_http::Client as DiscordClient;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ChannelMarker, UserMarker},
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MATCH_REQUEST_PAUSE: Duration = Duration::from_millis(75);
const RANKED_SOLO_QUEUE_ID: u16 = 420;
const RANKED_SOLO_QUEUE_TYPE: &str = "RANKED_SOLO_5x5";
#[derive(Clone)]
struct Config {
    riot_api_key: String,
    discord_bot_token: String,
    discord_channel_id: Id<ChannelMarker>,
    schedule: Schedule,
    users: Vec<PlayerConfig>,
    post_on_startup: bool,
}

impl Config {
    fn from_env(schedule: Schedule, users: Vec<PlayerConfig>) -> Result<Option<Self>> {
        let Some(riot_api_key) = env_value("RIOT_API_KEY") else {
            info!("League rank bot is disabled; RIOT_API_KEY is not set");
            return Ok(None);
        };

        let discord_bot_token = require_env("DISCORD_BOT_TOKEN")?;
        let discord_channel_id = parse_channel_id(&require_env("DISCORD_CHANNEL_ID")?)?;
        Ok(Some(Self {
            riot_api_key,
            discord_bot_token,
            discord_channel_id,
            schedule,
            users,
            post_on_startup: env_flag("LEAGUE_RANK_POST_ON_STARTUP"),
        }))
    }
}

pub(crate) async fn initialize_from_env(
    schedule: Schedule,
    users: Vec<PlayerConfig>,
    database: Option<BotDatabase>,
) -> Result<Option<Arc<LeagueService>>> {
    let Some(config) = Config::from_env(schedule, users)? else {
        return Ok(None);
    };
    let database = database.ok_or_else(|| {
        Error::Config("Rank database was not initialized for the League worker".to_string())
    })?;
    let store = RankStore { database };
    let riot_client = RiotClient::new(config.riot_api_key.clone())
        .map_err(|err| Error::Config(format!("Failed to initialize Riot API client: {err}")))?;
    let service = Arc::new(LeagueService {
        riot_client,
        store,
        time_zone: config.schedule.time_zone,
        users: Arc::from(config.users.clone()),
    });

    info!(
        users = config.users.len(),
        time_zone = %config.schedule.time_zone,
        post_times = %config.schedule.formatted_post_times(),
        "Starting League rank bot"
    );
    tokio::spawn(run(config, Arc::clone(&service)));
    Ok(Some(service))
}

async fn run(config: Config, service: Arc<LeagueService>) {
    let discord_client = DiscordClient::new(config.discord_bot_token.clone());

    if config.post_on_startup {
        info!("Posting immediate League rank recaps for integration testing");
        post_all_reports(&service, &discord_client, &config).await;
    }

    loop {
        let now = Utc::now();
        let next_post = config.schedule.next_post_after(now);
        let wait = (next_post - now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(0));

        info!(
            next_post = %next_post.with_timezone(&config.schedule.time_zone),
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
pub(crate) struct LeagueService {
    riot_client: RiotClient,
    store: RankStore,
    time_zone: Tz,
    users: Arc<[PlayerConfig]>,
}

pub(crate) struct PreparedReport {
    pub(crate) message: String,
    pub(crate) allowed_mentions: AllowedMentions,
    current: RankSnapshot,
    record: MatchRecord,
}

impl LeagueService {
    pub(crate) async fn prepare_report(
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

    pub(crate) fn player_for_discord_user(
        &self,
        discord_user_id: Id<UserMarker>,
    ) -> Option<PlayerConfig> {
        self.users
            .iter()
            .find(|player| player.discord_user_id == Some(discord_user_id))
            .cloned()
    }
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

#[derive(Clone)]
struct RankStore {
    database: BotDatabase,
}

impl RankStore {
    #[cfg(test)]
    async fn new_local(path: &std::path::Path) -> Result<Self> {
        Ok(Self {
            database: BotDatabase::new_local(path).await?,
        })
    }

    async fn latest_snapshot(
        &self,
        player: &PlayerConfig,
        puuid: &str,
    ) -> WorkerResult<Option<RankSnapshot>> {
        let _guard = self.database.lock().await;
        self.database.pull().await?;
        let connection = self.database.connect().await?;
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
        let _guard = self.database.lock().await;
        let connection = self.database.connect().await?;
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
        self.database.push().await?;
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

fn riot_url(route: &str, path_segments: &[&str]) -> Url {
    let mut url = Url::parse(&format!("https://{route}.api.riotgames.com/"))
        .expect("validated Riot route produces a valid URL");
    url.path_segments_mut()
        .expect("Riot API URL supports path segments")
        .pop_if_empty()
        .extend(path_segments);
    url
}

fn parse_channel_id(value: &str) -> Result<Id<ChannelMarker>> {
    let value = value
        .parse::<u64>()
        .map_err(|_| Error::Config("DISCORD_CHANNEL_ID must be a positive integer".to_string()))?;
    Id::new_checked(value)
        .ok_or_else(|| Error::Config("DISCORD_CHANNEL_ID must be greater than zero".to_string()))
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

pub(crate) type WorkerResult<T> = std::result::Result<T, WorkerError>;

#[derive(Debug)]
pub(crate) enum WorkerError {
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
    use chrono::TimeZone;

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
