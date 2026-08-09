use crate::error::{Error, Result};
use chrono::{DateTime, Days, LocalResult, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use std::{collections::HashSet, fs, path::Path};
use twilight_model::id::{Id, marker::UserMarker};

pub(crate) const DEFAULT_CONFIG_PATH: &str = "config/league-valorant-rank.ron";

#[derive(Clone, Debug)]
pub(crate) struct Schedule {
    pub(crate) time_zone: Tz,
    pub(crate) post_times: Vec<NaiveTime>,
}

impl Schedule {
    pub(crate) fn next_post_after(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        let local_now = now.with_timezone(&self.time_zone);

        for day_offset in 0..=2 {
            let date = local_now
                .date_naive()
                .checked_add_days(Days::new(day_offset))
                .expect("next rank post date is representable");

            for post_time in &self.post_times {
                let local_time = date.and_time(*post_time);
                let candidate = match self.time_zone.from_local_datetime(&local_time) {
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

        unreachable!("a configured rank post exists within the next two days")
    }

    pub(crate) fn formatted_post_times(&self) -> String {
        self.post_times
            .iter()
            .map(|time| time.format("%H:%M").to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaguePlayerConfig {
    pub(crate) riot_id: String,
    pub(crate) game_name: String,
    pub(crate) tag_line: String,
    pub(crate) platform: String,
    pub(crate) regional_route: String,
    pub(crate) discord_user_id: Option<Id<UserMarker>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ValorantPlayerConfig {
    pub(crate) riot_id: String,
    pub(crate) game_name: String,
    pub(crate) tag_line: String,
    pub(crate) region: String,
    pub(crate) platform: String,
    pub(crate) discord_user_id: Option<Id<UserMarker>>,
}

pub(crate) struct GameConfig {
    pub(crate) schedule: Schedule,
    pub(crate) league_players: Vec<LeaguePlayerConfig>,
    pub(crate) valorant_player: ValorantPlayerConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    timezone: String,
    post_times: Vec<String>,
    league: FileLeagueConfig,
    valorant: FileValorantConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLeagueConfig {
    users: Vec<FileLeaguePlayerConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLeaguePlayerConfig {
    riot_id: String,
    platform: String,
    regional_route: String,
    #[serde(default)]
    discord_user_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileValorantConfig {
    riot_id: String,
    region: String,
    platform: String,
    #[serde(default)]
    discord_user_id: Option<u64>,
}

pub(crate) fn load() -> Result<GameConfig> {
    let config_path = env_value("RANK_BOT_CONFIG").unwrap_or_else(default_config_path);
    let source = fs::read_to_string(&config_path).map_err(|err| {
        Error::Config(format!(
            "Failed to read rank bot config {config_path}: {err}"
        ))
    })?;
    let file = ron::from_str::<FileConfig>(&source).map_err(|err| {
        Error::Config(format!(
            "Failed to parse rank bot config {config_path}: {err}"
        ))
    })?;
    validate(file)
}

fn validate(file: FileConfig) -> Result<GameConfig> {
    let time_zone = file.timezone.parse::<Tz>().map_err(|_| {
        Error::Config(format!(
            "Rank bot timezone must be an IANA time zone; got {}",
            file.timezone
        ))
    })?;
    let mut post_times = file
        .post_times
        .iter()
        .map(|value| {
            NaiveTime::parse_from_str(value, "%H:%M").map_err(|_| {
                Error::Config(format!(
                    "Rank bot post time must use 24-hour HH:MM format; got {value}"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    post_times.sort_unstable();
    post_times.dedup();
    if post_times.is_empty() {
        return Err(Error::Config(
            "Rank bot config must contain at least one post time".to_string(),
        ));
    }

    let league_players = validate_league_players(file.league.users)?;
    let valorant_player = validate_valorant_player(file.valorant)?;
    Ok(GameConfig {
        schedule: Schedule {
            time_zone,
            post_times,
        },
        league_players,
        valorant_player,
    })
}

fn validate_league_players(
    raw_players: Vec<FileLeaguePlayerConfig>,
) -> Result<Vec<LeaguePlayerConfig>> {
    if raw_players.is_empty() {
        return Err(Error::Config(
            "Rank bot config must contain at least one League user".to_string(),
        ));
    }

    let mut seen = HashSet::new();
    let mut seen_discord_users = HashSet::new();
    let mut players = Vec::with_capacity(raw_players.len());
    for raw in raw_players {
        let (game_name, tag_line) = parse_riot_id("League", &raw.riot_id)?;
        let platform = raw.platform.trim().to_ascii_lowercase();
        if !is_league_platform_route(&platform) {
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

        players.push(LeaguePlayerConfig {
            riot_id,
            game_name,
            tag_line,
            platform,
            regional_route,
            discord_user_id,
        });
    }
    Ok(players)
}

fn validate_valorant_player(raw: FileValorantConfig) -> Result<ValorantPlayerConfig> {
    let (game_name, tag_line) = parse_riot_id("Valorant", &raw.riot_id)?;
    let region = raw.region.trim().to_ascii_lowercase();
    if !matches!(region.as_str(), "ap" | "br" | "eu" | "kr" | "latam" | "na") {
        return Err(Error::Config(format!(
            "Valorant region must be one of ap, br, eu, kr, latam, or na; got {}",
            raw.region
        )));
    }
    let platform = raw.platform.trim().to_ascii_lowercase();
    if !matches!(platform.as_str(), "pc" | "console") {
        return Err(Error::Config(format!(
            "Valorant platform must be pc or console; got {}",
            raw.platform
        )));
    }
    let discord_user_id = match raw.discord_user_id {
        Some(value) => Some(Id::new_checked(value).ok_or_else(|| {
            Error::Config(format!(
                "Discord user ID for {} must be greater than zero",
                raw.riot_id
            ))
        })?),
        None => None,
    };
    Ok(ValorantPlayerConfig {
        riot_id: format!("{game_name}#{tag_line}"),
        game_name,
        tag_line,
        region,
        platform,
        discord_user_id,
    })
}

fn parse_riot_id(game: &str, value: &str) -> Result<(String, String)> {
    let value = value.trim();
    let Some((game_name, tag_line)) = value.rsplit_once('#') else {
        return Err(Error::Config(format!(
            "{game} Riot ID must use GameName#TagLine format; got {value}"
        )));
    };
    let game_name = game_name.trim();
    let tag_line = tag_line.trim();
    if game_name.is_empty() || tag_line.is_empty() {
        return Err(Error::Config(format!(
            "{game} Riot ID must include both a game name and tag line; got {value}"
        )));
    }
    Ok((game_name.to_string(), tag_line.to_string()))
}

fn is_league_platform_route(value: &str) -> bool {
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

fn default_config_path() -> String {
    let runtime_path = Path::new(DEFAULT_CONFIG_PATH);
    if runtime_path.exists() {
        return runtime_path.display().to_string();
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn parses_both_games_from_one_config() {
        let source = r#"(
            timezone: "America/Toronto",
            post_times: ["23:00", "09:00"],
            league: (users: [(
                riot_id: "liights#6957",
                platform: "NA1",
                regional_route: "americas",
            )]),
            valorant: (
                riot_id: "xRayzor#0031",
                region: "NA",
                platform: "PC",
                discord_user_id: Some(123456789),
            ),
        )"#;
        let config = validate(ron::from_str(source).unwrap()).unwrap();

        assert_eq!(config.schedule.formatted_post_times(), "09:00, 23:00");
        assert_eq!(config.league_players[0].riot_id, "liights#6957");
        assert_eq!(config.valorant_player.riot_id, "xRayzor#0031");
        assert_eq!(config.valorant_player.region, "na");
        assert_eq!(
            config.valorant_player.discord_user_id,
            Some(Id::new(123456789))
        );
    }

    #[test]
    fn schedules_configured_times_across_daylight_saving() {
        let schedule = Schedule {
            time_zone: chrono_tz::America::Toronto,
            post_times: vec![
                NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
                NaiveTime::from_hms_opt(23, 0, 0).unwrap(),
            ],
        };

        assert_eq!(
            schedule.next_post_after(utc(2026, 8, 9, 12, 59)),
            utc(2026, 8, 9, 13, 0)
        );
        assert_eq!(
            schedule.next_post_after(utc(2026, 12, 9, 13, 59)),
            utc(2026, 12, 9, 14, 0)
        );
    }

    #[test]
    fn checked_in_config_is_valid_ron() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_CONFIG_PATH);
        let source = fs::read_to_string(path).unwrap();
        validate(ron::from_str(&source).unwrap()).unwrap();
    }
}
