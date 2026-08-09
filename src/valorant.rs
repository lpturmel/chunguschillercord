use crate::{
    config::{Schedule, ValorantPlayerConfig},
    error::{Error, Result},
};
use chrono::Utc;
use reqwest::{Client as HenrikClient, StatusCode, Url};
use serde::Deserialize;
use std::{fmt, sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{error, info};
use twilight_http::Client as DiscordClient;
use twilight_model::{
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ChannelMarker, UserMarker},
    },
};

const HENRIKDEV_MMR_URL: &str = "https://api.henrikdev.xyz/valorant/v3/mmr/";
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
    api_key: String,
    player: ValorantPlayerConfig,
}

impl ValorantService {
    pub(crate) async fn prepare_report(
        &self,
        player: &ValorantPlayerConfig,
        include_mention: bool,
    ) -> WorkerResult<PreparedReport> {
        let rank = fetch_rank(&self.client, &self.api_key, player).await?;
        Ok(PreparedReport {
            message: format_rank_message(player, &rank, include_mention),
            allowed_mentions: allowed_mentions_for_player(player, include_mention),
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
}

pub(crate) fn initialize_from_env(
    schedule: Schedule,
    player: ValorantPlayerConfig,
) -> Result<Option<Arc<ValorantService>>> {
    let Some(api_key) = env_value("HENRIKDEV_API_KEY") else {
        info!("Valorant rank bot is disabled; HENRIKDEV_API_KEY is not set");
        return Ok(None);
    };
    let discord_bot_token = require_value("DISCORD_BOT_TOKEN", env_value("DISCORD_BOT_TOKEN"))?;
    let discord_channel_id = parse_channel_id(&require_value(
        "DISCORD_CHANNEL_ID",
        env_value("DISCORD_CHANNEL_ID"),
    )?)?;
    let client = HenrikClient::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("chunguschillercord/0.1 Valorant rank bot")
        .build()
        .map_err(|err| Error::Config(format!("Failed to initialize HenrikDev client: {err}")))?;
    let service = Arc::new(ValorantService {
        client,
        api_key,
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
        info!("Posting immediate Valorant rank message for integration testing");
        if let Err(err) = post_current_rank(&service, &discord_client, &config).await {
            error!("Failed to post startup Valorant rank: {err}");
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
            "Valorant rank post scheduled"
        );
        sleep(wait).await;

        if let Err(err) = post_current_rank(&service, &discord_client, &config).await {
            error!("Failed to post Valorant rank: {err}");
        }
    }
}

async fn post_current_rank(
    service: &ValorantService,
    discord_client: &DiscordClient,
    config: &WorkerConfig,
) -> std::result::Result<(), WorkerError> {
    let report = service.prepare_report(&service.player, true).await?;

    discord_client
        .create_message(config.discord_channel_id)
        .content(&report.message)
        .allowed_mentions(Some(&report.allowed_mentions))
        .await?;

    info!(
        riot_id = %service.player.riot_id,
        "Posted Valorant rank to Discord"
    );
    Ok(())
}

async fn fetch_rank(
    client: &HenrikClient,
    api_key: &str,
    player: &ValorantPlayerConfig,
) -> std::result::Result<CurrentRank, WorkerError> {
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

    let response = client
        .get(url)
        .header("Authorization", api_key)
        .send()
        .await?;
    let status = response.status();

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let detail = body.chars().take(500).collect();
        return Err(WorkerError::HenrikDev { status, detail });
    }

    let response = response.json::<MmrResponse>().await?;
    Ok(response.data.current)
}

fn format_rank_message(
    player: &ValorantPlayerConfig,
    rank: &CurrentRank,
    include_mention: bool,
) -> String {
    let last_change = match rank.last_change {
        Some(change) if change > 0 => format!(" (+{change} RR last game)"),
        Some(change) if change < 0 => format!(" ({change} RR last game)"),
        _ => String::new(),
    };

    let mention = include_mention
        .then_some(player.discord_user_id)
        .flatten()
        .map(|user_id| format!("\n<@{}>", user_id.get()))
        .unwrap_or_default();

    format!(
        "**{}** is currently **{} — {} RR**{last_change}.{mention}",
        player.riot_id, rank.tier.name, rank.rr
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
    current: CurrentRank,
}

#[derive(Debug, Deserialize)]
struct CurrentRank {
    tier: Tier,
    rr: i32,
    last_change: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct Tier {
    name: String,
}

type WorkerResult<T> = std::result::Result<T, WorkerError>;

#[derive(Debug)]
pub(crate) enum WorkerError {
    Request(reqwest::Error),
    HenrikDev { status: StatusCode, detail: String },
    Discord(twilight_http::Error),
}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(err) => write!(formatter, "HenrikDev request failed: {err}"),
            Self::HenrikDev { status, detail } => {
                write!(formatter, "HenrikDev returned {status}: {detail}")
            }
            Self::Discord(err) => write!(formatter, "Discord request failed: {err}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_positive_and_negative_rank_changes() {
        let player = ValorantPlayerConfig {
            riot_id: "xRayzor#0031".to_string(),
            game_name: "xRayzor".to_string(),
            tag_line: "0031".to_string(),
            region: "na".to_string(),
            platform: "pc".to_string(),
            discord_user_id: Some(Id::new(123_456_789)),
        };
        let positive = CurrentRank {
            tier: Tier {
                name: "Gold 2".to_string(),
            },
            rr: 63,
            last_change: Some(18),
        };
        let negative = CurrentRank {
            tier: Tier {
                name: "Gold 2".to_string(),
            },
            rr: 44,
            last_change: Some(-19),
        };

        assert_eq!(
            format_rank_message(&player, &positive, false),
            "**xRayzor#0031** is currently **Gold 2 — 63 RR** (+18 RR last game)."
        );
        assert_eq!(
            format_rank_message(&player, &negative, false),
            "**xRayzor#0031** is currently **Gold 2 — 44 RR** (-19 RR last game)."
        );
    }

    #[test]
    fn scheduled_report_mentions_only_the_configured_user() {
        let player = ValorantPlayerConfig {
            riot_id: "xRayzor#0031".to_string(),
            game_name: "xRayzor".to_string(),
            tag_line: "0031".to_string(),
            region: "na".to_string(),
            platform: "pc".to_string(),
            discord_user_id: Some(Id::new(123_456_789)),
        };
        let rank = CurrentRank {
            tier: Tier {
                name: "Gold 2".to_string(),
            },
            rr: 63,
            last_change: Some(18),
        };

        let message = format_rank_message(&player, &rank, true);
        let allowed_mentions = allowed_mentions_for_player(&player, true);

        assert!(message.ends_with("\n<@123456789>"));
        assert_eq!(allowed_mentions.users, vec![Id::new(123_456_789)]);
        assert!(allowed_mentions.roles.is_empty());
        assert!(allowed_mentions.parse.is_empty());
    }

    #[test]
    fn parses_the_henrikdev_v3_rank_shape() {
        let response: MmrResponse = serde_json::from_str(
            r#"{
                "data": {
                    "current": {
                        "tier": { "id": 12, "name": "Gold 1" },
                        "rr": 20,
                        "last_change": -16,
                        "elo": 920
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(response.data.current.tier.name, "Gold 1");
        assert_eq!(response.data.current.rr, 20);
        assert_eq!(response.data.current.last_change, Some(-16));
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
