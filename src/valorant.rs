use crate::{
    config::{Schedule, ValorantPlayerConfig},
    database::BotDatabase,
    error::{Error, Result},
};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use chrono_tz::Tz;
use reqwest::{Client as HttpClient, StatusCode, Url, header::RETRY_AFTER};
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashSet, fmt, sync::Arc, time::Duration};
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

const HENRIKDEV_MMR_URL: &str = "https://api.henrikdev.xyz/valorant/v3/mmr/";
const HENRIKDEV_MATCHES_URL: &str = "https://api.henrikdev.xyz/valorant/v4/by-puuid/matches/";
const MATCH_PAGE_SIZE: usize = 10;
const MAX_MATCH_PAGES: usize = 10;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct WorkerConfig {
    discord_bot_token: String,
    discord_channel_id: Id<ChannelMarker>,
    schedule: Schedule,
    post_on_startup: bool,
}

pub(crate) struct ValorantService {
    client: HenrikClient,
    store: RankStore,
    time_zone: Tz,
    player: ValorantPlayerConfig,
}

impl ValorantService {
    pub(crate) async fn prepare_report(
        &self,
        player: &ValorantPlayerConfig,
        include_mention: bool,
    ) -> WorkerResult<PreparedReport> {
        let captured_at = Utc::now();
        let mmr = self.client.fetch_mmr(player).await?;
        let previous = self
            .store
            .latest_snapshot(player, &mmr.account.puuid)
            .await?;
        let record = match &previous {
            Some(snapshot) => {
                self.client
                    .fetch_competitive_record(
                        player,
                        &mmr.account.puuid,
                        snapshot.captured_at,
                        captured_at,
                    )
                    .await?
            }
            None => MatchRecord::default(),
        };
        let current = RankSnapshot {
            riot_id: player.riot_id.clone(),
            puuid: mmr.account.puuid,
            region: player.region.clone(),
            platform: player.platform.clone(),
            captured_at,
            rank: mmr.current,
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
    ) -> Option<ValorantPlayerConfig> {
        (self.player.discord_user_id == Some(discord_user_id)).then(|| self.player.clone())
    }
}

pub(crate) struct PreparedReport {
    pub(crate) message: String,
    pub(crate) allowed_mentions: AllowedMentions,
    current: RankSnapshot,
    record: MatchRecord,
}

pub(crate) fn initialize_from_env(
    schedule: Schedule,
    player: ValorantPlayerConfig,
    database: Option<BotDatabase>,
) -> Result<Option<Arc<ValorantService>>> {
    let Some(api_key) = env_value("HENRIKDEV_API_KEY") else {
        info!("Valorant rank bot is disabled; HENRIKDEV_API_KEY is not set");
        return Ok(None);
    };
    let database = database.ok_or_else(|| {
        Error::Config("Rank database was not initialized for the Valorant worker".to_string())
    })?;
    let discord_bot_token = require_value("DISCORD_BOT_TOKEN", env_value("DISCORD_BOT_TOKEN"))?;
    let discord_channel_id = parse_channel_id(&require_value(
        "DISCORD_CHANNEL_ID",
        env_value("DISCORD_CHANNEL_ID"),
    )?)?;
    let client = HenrikClient::new(api_key)?;
    let service = Arc::new(ValorantService {
        client,
        store: RankStore { database },
        time_zone: schedule.time_zone,
        player,
    });
    let config = WorkerConfig {
        discord_bot_token,
        discord_channel_id,
        schedule,
        post_on_startup: env_flag("VALORANT_RANK_POST_ON_STARTUP"),
    };

    info!(
        riot_id = %service.player.riot_id,
        region = %service.player.region,
        platform = %service.player.platform,
        time_zone = %config.schedule.time_zone,
        post_times = %config.schedule.formatted_post_times(),
        "Starting Valorant rank bot"
    );

    tokio::spawn(run(config, Arc::clone(&service)));
    Ok(Some(service))
}

async fn run(config: WorkerConfig, service: Arc<ValorantService>) {
    let discord_client = DiscordClient::new(config.discord_bot_token.clone());

    if config.post_on_startup {
        info!("Posting immediate Valorant rank recap for integration testing");
        if let Err(err) = post_rank_report(&service, &discord_client, &config).await {
            error!("Failed to post startup Valorant rank recap: {err}");
        }
    }

    loop {
        let now = Utc::now();
        let next_post = config.schedule.next_post_after(now);
        let wait = (next_post - now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(0));

        info!(
            next_post = %next_post.with_timezone(&config.schedule.time_zone),
            "Valorant rank recap scheduled"
        );
        sleep(wait).await;

        if let Err(err) = post_rank_report(&service, &discord_client, &config).await {
            error!("Failed to post Valorant rank recap: {err}");
        }
    }
}

async fn post_rank_report(
    service: &ValorantService,
    discord_client: &DiscordClient,
    config: &WorkerConfig,
) -> WorkerResult<()> {
    let report = service.prepare_report(&service.player, true).await?;

    discord_client
        .create_message(config.discord_channel_id)
        .content(&report.message)
        .allowed_mentions(Some(&report.allowed_mentions))
        .await?;

    service.store.save_snapshot(&report.current).await?;
    info!(
        riot_id = %service.player.riot_id,
        rank = %report.current.rank.tier.name,
        rr = report.current.rank.rr,
        wins = report.record.wins,
        losses = report.record.losses,
        draws = report.record.draws,
        "Posted Valorant rank recap to Discord"
    );
    Ok(())
}

#[derive(Clone)]
struct HenrikClient {
    http: HttpClient,
    api_key: String,
}

impl HenrikClient {
    fn new(api_key: String) -> Result<Self> {
        let http = HttpClient::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent("chunguschillercord/0.1 Valorant rank bot")
            .build()
            .map_err(|err| {
                Error::Config(format!("Failed to initialize HenrikDev client: {err}"))
            })?;
        Ok(Self { http, api_key })
    }

    async fn fetch_mmr(&self, player: &ValorantPlayerConfig) -> WorkerResult<MmrData> {
        let mut url = Url::parse(HENRIKDEV_MMR_URL).expect("HenrikDev MMR URL is valid");
        url.path_segments_mut()
            .expect("HenrikDev MMR URL supports path segments")
            .pop_if_empty()
            .extend([
                player.region.as_str(),
                player.platform.as_str(),
                player.game_name.as_str(),
                player.tag_line.as_str(),
            ]);

        Ok(self.get_json::<MmrResponse>(url).await?.data)
    }

    async fn fetch_competitive_record(
        &self,
        player: &ValorantPlayerConfig,
        puuid: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> WorkerResult<MatchRecord> {
        if start >= end {
            return Ok(MatchRecord::default());
        }

        let mut record = MatchRecord::default();
        let mut seen_matches = HashSet::new();
        for page in 0..MAX_MATCH_PAGES {
            let mut url =
                Url::parse(HENRIKDEV_MATCHES_URL).expect("HenrikDev match-history URL is valid");
            url.path_segments_mut()
                .expect("HenrikDev match-history URL supports path segments")
                .pop_if_empty()
                .extend([player.region.as_str(), player.platform.as_str(), puuid]);
            url.query_pairs_mut()
                .append_pair("mode", "competitive")
                .append_pair("size", &MATCH_PAGE_SIZE.to_string())
                .append_pair("start", &(page * MATCH_PAGE_SIZE).to_string());

            let matches = self.get_json::<MatchHistoryResponse>(url).await?.data;
            let page_size = matches.len();
            let mut reached_snapshot = false;

            for match_data in matches {
                let completed_at = match_completed_at(&match_data.metadata)?;
                if completed_at <= start {
                    reached_snapshot = true;
                    continue;
                }
                if completed_at > end
                    || !match_data.metadata.is_completed
                    || !seen_matches.insert(match_data.metadata.match_id.clone())
                {
                    continue;
                }

                match match_outcome(&match_data, puuid, &player.riot_id)? {
                    MatchOutcome::Win => record.wins += 1,
                    MatchOutcome::Loss => record.losses += 1,
                    MatchOutcome::Draw => record.draws += 1,
                }
            }

            if page_size < MATCH_PAGE_SIZE || reached_snapshot {
                return Ok(record);
            }
        }

        Err(WorkerError::Data(format!(
            "HenrikDev returned more than {} recent competitive matches for {}; refusing to report an incomplete interval",
            MATCH_PAGE_SIZE * MAX_MATCH_PAGES,
            player.riot_id
        )))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> WorkerResult<T> {
        for attempt in 0..=1 {
            let response = self
                .http
                .get(url.clone())
                .header("Authorization", &self.api_key)
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
                warn!(
                    retry_after,
                    "HenrikDev rate limit reached; retrying request"
                );
                sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(WorkerError::HenrikDev {
                    status,
                    detail: body.chars().take(500).collect(),
                });
            }

            return Ok(response.json::<T>().await?);
        }

        unreachable!("HenrikDev request either returns or retries once")
    }
}

fn match_completed_at(metadata: &MatchMetadata) -> WorkerResult<DateTime<Utc>> {
    let started_at = DateTime::parse_from_rfc3339(&metadata.started_at)?.with_timezone(&Utc);
    Ok(started_at
        + ChronoDuration::milliseconds(metadata.game_length_in_ms.unwrap_or_default().max(0)))
}

fn match_outcome(match_data: &MatchData, puuid: &str, riot_id: &str) -> WorkerResult<MatchOutcome> {
    let player = match_data
        .players
        .iter()
        .find(|player| player.puuid == puuid)
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "HenrikDev match {} did not contain configured player {riot_id}",
                match_data.metadata.match_id
            ))
        })?;
    let team = match_data
        .teams
        .iter()
        .find(|team| team.team_id == player.team_id)
        .ok_or_else(|| {
            WorkerError::Data(format!(
                "HenrikDev match {} did not contain team {} for {riot_id}",
                match_data.metadata.match_id, player.team_id
            ))
        })?;

    if team.won {
        Ok(MatchOutcome::Win)
    } else if match_data
        .teams
        .iter()
        .any(|other_team| other_team.team_id != team.team_id && other_team.won)
    {
        Ok(MatchOutcome::Loss)
    } else {
        Ok(MatchOutcome::Draw)
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
        player: &ValorantPlayerConfig,
        puuid: &str,
    ) -> WorkerResult<Option<RankSnapshot>> {
        let _guard = self.database.lock().await;
        self.database.pull().await?;
        let connection = self.database.connect().await?;
        let mut rows = connection
            .query(
                "SELECT riot_id, puuid, region, platform, captured_at, tier, rr, elo
                 FROM valorant_rank_snapshots
                 WHERE region = ? AND platform = ? AND puuid = ?
                 ORDER BY captured_at DESC, id DESC
                 LIMIT 1",
                (player.region.as_str(), player.platform.as_str(), puuid),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let captured_at = DateTime::parse_from_rfc3339(&row.get::<String>(4)?)?.with_timezone(&Utc);

        Ok(Some(RankSnapshot {
            riot_id: row.get::<String>(0)?,
            puuid: row.get::<String>(1)?,
            region: row.get::<String>(2)?,
            platform: row.get::<String>(3)?,
            captured_at,
            rank: Rank {
                tier: Tier {
                    name: row.get::<String>(5)?,
                },
                rr: row.get::<i64>(6)? as i32,
                elo: row.get::<i64>(7)? as i32,
                last_change: None,
            },
        }))
    }

    async fn save_snapshot(&self, snapshot: &RankSnapshot) -> WorkerResult<()> {
        let _guard = self.database.lock().await;
        let connection = self.database.connect().await?;
        connection
            .execute(
                "INSERT INTO valorant_rank_snapshots
                 (riot_id, puuid, region, platform, captured_at, tier, rr, elo)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                turso::params![
                    snapshot.riot_id.clone(),
                    snapshot.puuid.clone(),
                    snapshot.region.clone(),
                    snapshot.platform.clone(),
                    snapshot.captured_at.to_rfc3339(),
                    snapshot.rank.tier.name.clone(),
                    snapshot.rank.rr as i64,
                    snapshot.rank.elo as i64,
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
    region: String,
    platform: String,
    captured_at: DateTime<Utc>,
    rank: Rank,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Rank {
    tier: Tier,
    rr: i32,
    elo: i32,
    last_change: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Tier {
    name: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MatchRecord {
    wins: u32,
    losses: u32,
    draws: u32,
}

impl MatchRecord {
    fn win_rate(self) -> Option<f64> {
        let games = self.wins + self.losses + self.draws;
        (games > 0).then(|| self.wins as f64 / games as f64 * 100.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchOutcome {
    Win,
    Loss,
    Draw,
}

fn format_report(
    player: &ValorantPlayerConfig,
    previous: Option<&RankSnapshot>,
    current: &RankSnapshot,
    record: MatchRecord,
    time_zone: Tz,
    include_mention: bool,
) -> String {
    let local_now = current.captured_at.with_timezone(&time_zone);
    let date = local_now.format("%A, %B %-d, %Y");
    let updated_at = local_now.format("%A, %B %-d, %Y at %H:%M:%S %Z");
    let mention = include_mention
        .then_some(player.discord_user_id)
        .flatten()
        .map(|user_id| format!("\n<@{}>", user_id.get()))
        .unwrap_or_default();
    let last_change = current
        .rank
        .last_change
        .map(|change| format!(" · **{} RR last game**", signed_number(change)))
        .unwrap_or_default();
    let record_label = if record.draws > 0 {
        format!(
            "**{}W / {}L / {}D**",
            record.wins, record.losses, record.draws
        )
    } else {
        format!("**{}W / {}L**", record.wins, record.losses)
    };
    let record_line = match record.win_rate() {
        Some(win_rate) => format!("**Record:** {record_label} · **{win_rate:.2}% win rate**"),
        None if previous.is_some() => "**Record:** **0W / 0L** · no competitive games".to_string(),
        None => "**Record:** baseline created · no previous snapshot".to_string(),
    };
    let (start_line, net_line, interval_line) = match previous {
        Some(previous) => {
            let net = current.rank.elo - previous.rank.elo;
            let interval_start = previous
                .captured_at
                .with_timezone(&time_zone)
                .format("%b %-d at %H:%M %Z");
            (
                format!(
                    "> **Start:** {} · {} RR",
                    previous.rank.tier.name, previous.rank.rr
                ),
                format!("> **Net:** **{} RR**", signed_number(net)),
                format!("-# Interval began {interval_start}"),
            )
        }
        None => (
            "> **Start:** unavailable (first snapshot)".to_string(),
            "> **Net:** baseline created".to_string(),
            "-# The next scheduled post will include a complete RR recap.".to_string(),
        ),
    };

    format!(
        "## 🎯 Valorant Ranked Recap\n\
         ### {}{mention}\n\
         **Current:** **{}** · **{} RR**{last_change}\n\
         \n\
         **Recap for {date}**\n\
         {record_line}\n\
         \n\
         **RR recap**\n\
         {start_line}\n\
         > **End:** {} · {} RR\n\
         {net_line}\n\
         \n\
         -# Updated {updated_at}\n\
         {interval_line}",
        escape_discord_markdown(&player.riot_id),
        current.rank.tier.name,
        current.rank.rr,
        current.rank.tier.name,
        current.rank.rr,
    )
}

fn allowed_mentions_for_player(
    player: &ValorantPlayerConfig,
    include_mention: bool,
) -> AllowedMentions {
    AllowedMentions {
        users: if include_mention {
            player.discord_user_id.iter().copied().collect()
        } else {
            Vec::new()
        },
        ..AllowedMentions::default()
    }
}

fn signed_number(value: i32) -> String {
    if value > 0 {
        format!("+{value}")
    } else {
        value.to_string()
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

fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn require_value(name: &str, value: Option<String>) -> Result<String> {
    value.ok_or_else(|| Error::Config(format!("Missing env var {name}")))
}

fn parse_channel_id(value: &str) -> Result<Id<ChannelMarker>> {
    let value = value
        .parse::<u64>()
        .map_err(|_| Error::Config("DISCORD_CHANNEL_ID must be a positive integer".to_string()))?;
    Id::new_checked(value)
        .ok_or_else(|| Error::Config("DISCORD_CHANNEL_ID must be greater than zero".to_string()))
}

fn env_flag(name: &str) -> bool {
    env_value(name).is_some_and(|value| parse_env_flag(&value))
}

fn parse_env_flag(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

#[derive(Debug, Deserialize)]
struct MmrResponse {
    data: MmrData,
}

#[derive(Debug, Deserialize)]
struct MmrData {
    account: MmrAccount,
    current: Rank,
}

#[derive(Debug, Deserialize)]
struct MmrAccount {
    puuid: String,
}

#[derive(Debug, Deserialize)]
struct MatchHistoryResponse {
    data: Vec<MatchData>,
}

#[derive(Debug, Deserialize)]
struct MatchData {
    metadata: MatchMetadata,
    players: Vec<MatchPlayer>,
    teams: Vec<MatchTeam>,
}

#[derive(Debug, Deserialize)]
struct MatchMetadata {
    match_id: String,
    started_at: String,
    game_length_in_ms: Option<i64>,
    is_completed: bool,
}

#[derive(Debug, Deserialize)]
struct MatchPlayer {
    puuid: String,
    team_id: String,
}

#[derive(Debug, Deserialize)]
struct MatchTeam {
    team_id: String,
    won: bool,
}

pub(crate) type WorkerResult<T> = std::result::Result<T, WorkerError>;

#[derive(Debug)]
pub(crate) enum WorkerError {
    Request(reqwest::Error),
    HenrikDev { status: StatusCode, detail: String },
    Discord(twilight_http::Error),
    Database(turso::Error),
    Timestamp(chrono::ParseError),
    Data(String),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(err) => write!(formatter, "HenrikDev request failed: {err}"),
            Self::HenrikDev { status, detail } => {
                write!(formatter, "HenrikDev returned {status}: {detail}")
            }
            Self::Discord(err) => write!(formatter, "Discord request failed: {err}"),
            Self::Database(err) => write!(formatter, "Valorant snapshot database failed: {err}"),
            Self::Timestamp(err) => write!(formatter, "Invalid Valorant timestamp: {err}"),
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

    fn player() -> ValorantPlayerConfig {
        ValorantPlayerConfig {
            riot_id: "xRayzor#0031".to_string(),
            game_name: "xRayzor".to_string(),
            tag_line: "0031".to_string(),
            region: "na".to_string(),
            platform: "pc".to_string(),
            discord_user_id: Some(Id::new(123_456_789)),
        }
    }

    fn rank(tier: &str, rr: i32, elo: i32, last_change: Option<i32>) -> Rank {
        Rank {
            tier: Tier {
                name: tier.to_string(),
            },
            rr,
            elo,
            last_change,
        }
    }

    fn snapshot(captured_at: DateTime<Utc>, rank: Rank) -> RankSnapshot {
        RankSnapshot {
            riot_id: "xRayzor#0031".to_string(),
            puuid: "configured-player".to_string(),
            region: "na".to_string(),
            platform: "pc".to_string(),
            captured_at,
            rank,
        }
    }

    #[test]
    fn formats_complete_markdown_recap_with_last_game_rr() {
        let previous = snapshot(utc(2026, 8, 9, 13, 0), rank("Gold 1", 80, 980, None));
        let current = snapshot(utc(2026, 8, 10, 3, 0), rank("Gold 2", 44, 1044, Some(-19)));
        let message = format_report(
            &player(),
            Some(&previous),
            &current,
            MatchRecord {
                wins: 5,
                losses: 1,
                draws: 0,
            },
            chrono_tz::America::Toronto,
            false,
        );

        assert!(message.contains("## 🎯 Valorant Ranked Recap"));
        assert!(message.contains("**Current:** **Gold 2** · **44 RR** · **-19 RR last game**"));
        assert!(message.contains("**5W / 1L** · **83.33% win rate**"));
        assert!(message.contains("> **Start:** Gold 1 · 80 RR"));
        assert!(message.contains("> **End:** Gold 2 · 44 RR"));
        assert!(message.contains("> **Net:** **+64 RR**"));
        assert!(message.contains("Sunday, August 9, 2026"));
    }

    #[test]
    fn formats_first_report_as_a_baseline() {
        let current = snapshot(utc(2026, 8, 9, 13, 0), rank("Gold 2", 63, 1063, Some(18)));
        let message = format_report(
            &player(),
            None,
            &current,
            MatchRecord::default(),
            chrono_tz::America::Toronto,
            false,
        );

        assert!(message.contains("**Record:** baseline created · no previous snapshot"));
        assert!(message.contains("> **Start:** unavailable (first snapshot)"));
        assert!(message.contains("> **Net:** baseline created"));
        assert!(message.contains("**+18 RR last game**"));
    }

    #[test]
    fn scheduled_report_mentions_only_the_configured_user() {
        let current = snapshot(utc(2026, 8, 9, 13, 0), rank("Gold 2", 63, 1063, Some(18)));
        let message = format_report(
            &player(),
            None,
            &current,
            MatchRecord::default(),
            chrono_tz::America::Toronto,
            true,
        );
        let allowed_mentions = allowed_mentions_for_player(&player(), true);

        assert!(message.contains("<@123456789>"));
        assert_eq!(allowed_mentions.users, vec![Id::new(123_456_789)]);
        assert!(allowed_mentions.roles.is_empty());
        assert!(allowed_mentions.parse.is_empty());
    }

    #[test]
    fn manual_report_does_not_mention_the_invoking_user() {
        let current = snapshot(utc(2026, 8, 9, 13, 0), rank("Gold 2", 63, 1063, Some(18)));
        let message = format_report(
            &player(),
            None,
            &current,
            MatchRecord::default(),
            chrono_tz::America::Toronto,
            false,
        );
        let allowed_mentions = allowed_mentions_for_player(&player(), false);

        assert!(!message.contains("<@123456789>"));
        assert!(allowed_mentions.users.is_empty());
    }

    #[test]
    fn parses_current_mmr_and_v4_match_shapes() {
        let response: MmrResponse = serde_json::from_str(
            r#"{
                "data": {
                    "account": {"puuid": "configured-player", "name": "xRayzor", "tag": "0031"},
                    "current": {
                        "tier": {"id": 12, "name": "Gold 1"},
                        "rr": 20,
                        "last_change": -16,
                        "elo": 920
                    }
                }
            }"#,
        )
        .unwrap();
        let matches: MatchHistoryResponse = serde_json::from_str(
            r#"{
                "status": 200,
                "data": [{
                    "metadata": {
                        "match_id": "match-1",
                        "started_at": "2026-08-09T15:00:00Z",
                        "game_length_in_ms": 1800000,
                        "is_completed": true
                    },
                    "players": [
                        {"puuid": "configured-player", "team_id": "Red"},
                        {"puuid": "other-player", "team_id": "Blue"}
                    ],
                    "teams": [
                        {"team_id": "Red", "won": true},
                        {"team_id": "Blue", "won": false}
                    ]
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(response.data.account.puuid, "configured-player");
        assert_eq!(response.data.current.tier.name, "Gold 1");
        assert_eq!(response.data.current.last_change, Some(-16));
        assert_eq!(
            match_outcome(&matches.data[0], "configured-player", "xRayzor#0031").unwrap(),
            MatchOutcome::Win
        );
    }

    #[test]
    fn distinguishes_losses_and_draws() {
        let loss: MatchHistoryResponse = serde_json::from_str(
            r#"{"data":[{
                "metadata":{"match_id":"loss","started_at":"2026-08-09T15:00:00Z","game_length_in_ms":1800000,"is_completed":true},
                "players":[{"puuid":"configured-player","team_id":"Red"}],
                "teams":[{"team_id":"Red","won":false},{"team_id":"Blue","won":true}]
            }]}"#,
        )
        .unwrap();
        let draw: MatchHistoryResponse = serde_json::from_str(
            r#"{"data":[{
                "metadata":{"match_id":"draw","started_at":"2026-08-09T15:00:00Z","game_length_in_ms":1800000,"is_completed":true},
                "players":[{"puuid":"configured-player","team_id":"Red"}],
                "teams":[{"team_id":"Red","won":false},{"team_id":"Blue","won":false}]
            }]}"#,
        )
        .unwrap();

        assert_eq!(
            match_outcome(&loss.data[0], "configured-player", "xRayzor#0031").unwrap(),
            MatchOutcome::Loss
        );
        assert_eq!(
            match_outcome(&draw.data[0], "configured-player", "xRayzor#0031").unwrap(),
            MatchOutcome::Draw
        );
    }

    #[test]
    fn attributes_a_match_to_the_interval_when_it_finishes() {
        let metadata = MatchMetadata {
            match_id: "crossing-snapshot".to_string(),
            started_at: "2026-08-09T12:50:00Z".to_string(),
            game_length_in_ms: Some(20 * 60 * 1000),
            is_completed: true,
        };

        let completed_at = match_completed_at(&metadata).unwrap();
        assert!(completed_at > utc(2026, 8, 9, 13, 0));
        assert_eq!(completed_at, utc(2026, 8, 9, 13, 10));
    }

    #[tokio::test]
    async fn persists_and_loads_the_latest_snapshot() {
        let database_path = std::env::temp_dir().join(format!(
            "chunguschillercord-valorant-rank-test-{}-{}.db",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let store = RankStore::new_local(&database_path).await.unwrap();
        let first = snapshot(utc(2026, 8, 9, 13, 0), rank("Gold 1", 80, 980, Some(15)));
        let second = snapshot(utc(2026, 8, 10, 3, 0), rank("Gold 2", 44, 1044, Some(-19)));

        store.save_snapshot(&first).await.unwrap();
        store.save_snapshot(&second).await.unwrap();

        let loaded = store
            .latest_snapshot(&player(), "configured-player")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.captured_at, second.captured_at);
        assert_eq!(loaded.rank.tier.name, "Gold 2");
        assert_eq!(loaded.rank.rr, 44);
        assert_eq!(loaded.rank.elo, 1044);
        assert_eq!(loaded.rank.last_change, None);

        drop(store);
        std::fs::remove_file(database_path).unwrap();
    }

    #[test]
    fn startup_flag_accepts_common_truthy_values() {
        for value in ["1", "true", "TRUE", "yes", "YeS"] {
            assert!(parse_env_flag(value));
        }

        assert!(!parse_env_flag("0"));
        assert!(!parse_env_flag("false"));
    }
}
