use crate::error::{Error, Result};
use chrono::Utc;
use std::{fs, sync::Arc};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use tracing::info;

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
    Migration {
        version: 3,
        name: "create_valorant_rank_snapshots",
        sql: include_str!("../migrations/0003_create_valorant_rank_snapshots.sql"),
    },
    Migration {
        version: 4,
        name: "create_wow_keystone_snapshots",
        sql: include_str!("../migrations/0004_create_wow_keystone_snapshots.sql"),
    },
];

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

#[derive(Clone)]
enum Backend {
    #[cfg(test)]
    Local(turso::Database),
    Synced(turso::sync::Database),
}

#[derive(Clone)]
pub(crate) struct BotDatabase {
    backend: Backend,
    sync_lock: Arc<AsyncMutex<()>>,
}

impl BotDatabase {
    pub(crate) async fn initialize_from_env() -> Result<Option<Self>> {
        if [
            "RIOT_API_KEY",
            "HENRIKDEV_API_KEY",
            "TURSO_DATABASE_URL",
            "TURSO_AUTH_TOKEN",
        ]
        .iter()
        .all(|name| env_value(name).is_none())
        {
            return Ok(None);
        }

        Ok(Some(Self::required_from_env().await?))
    }

    /// Companion uses the already configured database independently of rank APIs.
    pub(crate) async fn required_from_env() -> Result<Self> {
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
        let database = Self::new(Backend::Synced(database)).await?;
        info!(%database_path, "Turso database is ready");
        Ok(database)
    }

    #[cfg(test)]
    pub(crate) async fn new_local(path: &std::path::Path) -> Result<Self> {
        let path = path
            .to_str()
            .ok_or_else(|| Error::Config("Test database path is not valid UTF-8".to_string()))?;
        let database = turso::Builder::new_local(path).build().await?;
        Self::new(Backend::Local(database)).await
    }

    async fn new(backend: Backend) -> Result<Self> {
        let database = Self {
            backend,
            sync_lock: Arc::new(AsyncMutex::new(())),
        };
        database.run_migrations().await?;
        Ok(database)
    }

    async fn run_migrations(&self) -> Result<()> {
        let _guard = self.lock().await;
        self.pull().await?;
        let connection = self.connect().await?;

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
        self.push().await?;
        Ok(())
    }

    pub(crate) async fn lock(&self) -> MutexGuard<'_, ()> {
        self.sync_lock.lock().await
    }

    pub(crate) async fn connect(&self) -> turso::Result<turso::Connection> {
        match &self.backend {
            #[cfg(test)]
            Backend::Local(database) => database.connect(),
            Backend::Synced(database) => database.connect().await,
        }
    }

    pub(crate) async fn pull(&self) -> turso::Result<()> {
        match &self.backend {
            Backend::Synced(database) => {
                database.pull().await?;
            }
            #[cfg(test)]
            Backend::Local(_) => {}
        }
        Ok(())
    }

    pub(crate) async fn push(&self) -> turso::Result<()> {
        match &self.backend {
            Backend::Synced(database) => {
                database.push().await?;
            }
            #[cfg(test)]
            Backend::Local(_) => {}
        }
        Ok(())
    }
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
