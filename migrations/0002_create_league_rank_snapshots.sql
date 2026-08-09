CREATE TABLE IF NOT EXISTS league_rank_snapshots (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  riot_id TEXT NOT NULL,
  puuid TEXT NOT NULL,
  platform_route TEXT NOT NULL,
  captured_at TEXT NOT NULL,
  tier TEXT NOT NULL,
  division TEXT NOT NULL DEFAULT '',
  league_points INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS league_rank_snapshots_player_time_idx
  ON league_rank_snapshots(platform_route, puuid, captured_at DESC);
