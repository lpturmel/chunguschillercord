use crate::error::{Error, Result};
use chrono::{DateTime, Days, LocalResult, TimeZone, Utc};
use chrono_tz::Tz;
use reqwest::{Client as HenrikClient, StatusCode, Url};
use serde::Deserialize;
use std::{fmt, time::Duration};
use tokio::time::sleep;
use tracing::{error, info};
use twilight_http::Client as DiscordClient;
use twilight_model::id::{Id, marker::ChannelMarker};

const HENRIKDEV_MMR_URL: &str = "https://api.henrikdev.xyz/valorant/v3/mmr/";
const DEFAULT_REGION: &str = "na";
const DEFAULT_PLATFORM: &str = "pc";
const DEFAULT_RIOT_NAME: &str = "xRayzor";
const DEFAULT_RIOT_TAG: &str = "0031";
const DEFAULT_TIME_ZONE: &str = "America/Toronto";
const POST_HOURS: [u32; 2] = [9, 23];
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct Config {
    henrikdev_api_key: String,
    discord_bot_token: String,
    discord_channel_id: Id<ChannelMarker>,
    region: String,
    platform: String,
    riot_name: String,
    riot_tag: String,
    time_zone: Tz,
    post_on_startup: bool,
}

impl Config {
    fn from_env() -> Result<Option<Self>> {
        let Some(henrikdev_api_key) = env_value("HENRIKDEV_API_KEY") else {
            info!("Valorant rank bot is disabled; HENRIKDEV_API_KEY is not set");
            return Ok(None);
        };

        let discord_bot_token = require_value("DISCORD_BOT_TOKEN", env_value("DISCORD_BOT_TOKEN"))?;
        let discord_channel_id =
            require_value("DISCORD_CHANNEL_ID", env_value("DISCORD_CHANNEL_ID"))?;
        let discord_channel_id = discord_channel_id.parse::<u64>().map_err(|_| {
            Error::Config("DISCORD_CHANNEL_ID must be a positive integer".to_string())
        })?;
        let discord_channel_id = Id::new_checked(discord_channel_id).ok_or_else(|| {
            Error::Config("DISCORD_CHANNEL_ID must be greater than zero".to_string())
        })?;

        let region = optional_env("VALORANT_REGION", DEFAULT_REGION);
        if !matches!(region.as_str(), "ap" | "br" | "eu" | "kr" | "latam" | "na") {
            return Err(Error::Config(format!(
                "VALORANT_REGION must be one of ap, br, eu, kr, latam, or na; got {region}"
            )));
        }

        let platform = optional_env("VALORANT_PLATFORM", DEFAULT_PLATFORM);
        if !matches!(platform.as_str(), "pc" | "console") {
            return Err(Error::Config(format!(
                "VALORANT_PLATFORM must be pc or console; got {platform}"
            )));
        }

        let time_zone_name = optional_env("VALORANT_RANK_TIME_ZONE", DEFAULT_TIME_ZONE);
        let time_zone = time_zone_name.parse::<Tz>().map_err(|_| {
            Error::Config(format!(
                "VALORANT_RANK_TIME_ZONE must be an IANA time zone; got {time_zone_name}"
            ))
        })?;

        Ok(Some(Self {
            henrikdev_api_key,
            discord_bot_token,
            discord_channel_id,
            region,
            platform,
            riot_name: optional_env("VALORANT_RIOT_NAME", DEFAULT_RIOT_NAME),
            riot_tag: optional_env("VALORANT_RIOT_TAG", DEFAULT_RIOT_TAG),
            time_zone,
            post_on_startup: env_flag("VALORANT_RANK_POST_ON_STARTUP"),
        }))
    }
}

pub fn start_from_env() -> Result<()> {
    let Some(config) = Config::from_env()? else {
        info!("Valorant rank bot is disabled; Discord and HenrikDev credentials are not set");
        return Ok(());
    };

    info!(
        riot_id = %format!("{}#{}", config.riot_name, config.riot_tag),
        region = %config.region,
        platform = %config.platform,
        time_zone = %config.time_zone,
        "Starting Valorant rank bot; posts are scheduled for 09:00 and 23:00"
    );

    tokio::spawn(run(config));
    Ok(())
}

async fn run(config: Config) {
    let henrik_client = match HenrikClient::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent("chunguschillercord/0.1 Valorant rank bot")
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            error!("Failed to initialize HenrikDev HTTP client: {err}");
            return;
        }
    };
    let discord_client = DiscordClient::new(config.discord_bot_token.clone());

    if config.post_on_startup {
        info!("Posting immediate Valorant rank message for integration testing");
        if let Err(err) = post_current_rank(&henrik_client, &discord_client, &config).await {
            error!("Failed to post startup Valorant rank: {err}");
        }
    }

    loop {
        let now = Utc::now();
        let next_post = next_post_after(now, config.time_zone);
        let wait = (next_post - now)
            .to_std()
            .unwrap_or_else(|_| Duration::from_secs(0));

        info!(
            next_post = %next_post.with_timezone(&config.time_zone),
            "Valorant rank post scheduled"
        );
        sleep(wait).await;

        if let Err(err) = post_current_rank(&henrik_client, &discord_client, &config).await {
            error!("Failed to post Valorant rank: {err}");
        }
    }
}

async fn post_current_rank(
    henrik_client: &HenrikClient,
    discord_client: &DiscordClient,
    config: &Config,
) -> std::result::Result<(), WorkerError> {
    let rank = fetch_rank(henrik_client, config).await?;
    let message = format_rank_message(config, &rank);

    discord_client
        .create_message(config.discord_channel_id)
        .content(&message)
        .await?;

    info!(
        riot_id = %format!("{}#{}", config.riot_name, config.riot_tag),
        tier = %rank.tier.name,
        rr = rank.rr,
        "Posted Valorant rank to Discord"
    );
    Ok(())
}

async fn fetch_rank(
    client: &HenrikClient,
    config: &Config,
) -> std::result::Result<CurrentRank, WorkerError> {
    let mut url = Url::parse(HENRIKDEV_MMR_URL).expect("HenrikDev MMR URL is valid");
    url.path_segments_mut()
        .expect("HenrikDev MMR URL supports path segments")
        .pop_if_empty()
        .extend([
            config.region.as_str(),
            config.platform.as_str(),
            config.riot_name.as_str(),
            config.riot_tag.as_str(),
        ]);

    let response = client
        .get(url)
        .header("Authorization", &config.henrikdev_api_key)
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

fn format_rank_message(config: &Config, rank: &CurrentRank) -> String {
    let riot_id = format!("{}#{}", config.riot_name, config.riot_tag);
    let last_change = match rank.last_change {
        Some(change) if change > 0 => format!(" (+{change} RR last game)"),
        Some(change) if change < 0 => format!(" ({change} RR last game)"),
        _ => String::new(),
    };

    format!(
        "**{riot_id}** is currently **{} — {} RR**{last_change}.",
        rank.tier.name, rank.rr
    )
}

fn next_post_after(now: DateTime<Utc>, time_zone: Tz) -> DateTime<Utc> {
    let local_now = now.with_timezone(&time_zone);

    for day_offset in 0..=1 {
        let date = local_now
            .date_naive()
            .checked_add_days(Days::new(day_offset))
            .expect("next rank post date is representable");

        for hour in POST_HOURS {
            let local_time = date
                .and_hms_opt(hour, 0, 0)
                .expect("rank post hour is valid");
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

    unreachable!("a scheduled Valorant rank post exists within the next day")
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

fn optional_env(name: &str, default: &str) -> String {
    env_value(name).unwrap_or_else(|| default.to_string())
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

#[derive(Debug)]
enum WorkerError {
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

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn schedules_nine_am_during_daylight_saving_time() {
        let next = next_post_after(utc(2026, 8, 6, 12, 59), chrono_tz::America::Toronto);
        assert_eq!(next, utc(2026, 8, 6, 13, 0));
    }

    #[test]
    fn schedules_eleven_pm_after_the_morning_post() {
        let next = next_post_after(utc(2026, 8, 6, 13, 1), chrono_tz::America::Toronto);
        assert_eq!(next, utc(2026, 8, 7, 3, 0));
    }

    #[test]
    fn schedules_nine_am_during_standard_time() {
        let next = next_post_after(utc(2026, 12, 6, 13, 59), chrono_tz::America::Toronto);
        assert_eq!(next, utc(2026, 12, 6, 14, 0));
    }

    #[test]
    fn schedules_next_morning_after_the_evening_post() {
        let next = next_post_after(utc(2026, 8, 7, 3, 1), chrono_tz::America::Toronto);
        assert_eq!(next, utc(2026, 8, 7, 13, 0));
    }

    #[test]
    fn formats_positive_and_negative_rank_changes() {
        let config = Config {
            henrikdev_api_key: "key".to_string(),
            discord_bot_token: "token".to_string(),
            discord_channel_id: Id::new(1),
            region: "na".to_string(),
            platform: "pc".to_string(),
            riot_name: "xRayzor".to_string(),
            riot_tag: "0031".to_string(),
            time_zone: chrono_tz::America::Toronto,
            post_on_startup: false,
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
            format_rank_message(&config, &positive),
            "**xRayzor#0031** is currently **Gold 2 — 63 RR** (+18 RR last game)."
        );
        assert_eq!(
            format_rank_message(&config, &negative),
            "**xRayzor#0031** is currently **Gold 2 — 44 RR** (-19 RR last game)."
        );
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
