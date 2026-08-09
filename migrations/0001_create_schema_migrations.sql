CREATE TABLE IF NOT EXISTS chunguschillercord_schema_migrations (
  version INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  applied_at TEXT NOT NULL
);
