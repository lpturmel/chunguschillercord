CREATE TABLE IF NOT EXISTS valorant_rank_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  riot_id TEXT NOT NULL,
  puuid TEXT NOT NULL,
  region TEXT NOT NULL,
  platform TEXT NOT NULL,
  captured_at TEXT NOT NULL,
  tier TEXT NOT NULL,
  rr INTEGER NOT NULL,
  elo INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS valorant_rank_snapshots_player_time_idx
  ON valorant_rank_snapshots(region, platform, puuid, captured_at DESC);
