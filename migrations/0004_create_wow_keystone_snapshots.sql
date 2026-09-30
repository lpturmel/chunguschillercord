CREATE TABLE IF NOT EXISTS wow_keystone_snapshots (
    character_name TEXT NOT NULL,
    realm TEXT NOT NULL,
    identity_key TEXT NOT NULL,
    observed_at INTEGER NOT NULL,
    reset_at INTEGER NOT NULL,
    key_level INTEGER NOT NULL CHECK (key_level BETWEEN 0 AND 100),
    challenge_map_id INTEGER NOT NULL CHECK (challenge_map_id >= 0),
    dungeon_name TEXT NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('OWN', 'PARTY', 'GUILD')),
    imported_at INTEGER NOT NULL,
    file_modified_at INTEGER NOT NULL,
    CHECK ((key_level = 0 AND challenge_map_id = 0) OR (key_level > 0 AND challenge_map_id > 0)),
    PRIMARY KEY (identity_key, observed_at, source)
);
CREATE INDEX IF NOT EXISTS wow_keystone_latest ON wow_keystone_snapshots(identity_key, observed_at DESC);
