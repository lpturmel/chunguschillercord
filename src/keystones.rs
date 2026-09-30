//! Owner-only SavedVariables import. This module never runs a Lua interpreter.
use crate::{
    database::BotDatabase,
    error::{Error, Result},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

const DEFAULT_RETAIL: &str = "/Applications/World of Warcraft/_retail_";
const DEFAULT_ALLOWLIST: &str = "config/wow-keystones.json";
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const STALE_AFTER: i64 = 6 * 60 * 60;
fn invalid(message: &str) -> Error {
    Error::Config(format!("Keystones: {message}"))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Character {
    name: String,
    realm: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Allowlist {
    characters: Vec<Character>,
}
impl Allowlist {
    fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)?;
        if bytes.len() > 65536 {
            return Err(invalid("allowlist too large"));
        }
        let list: Self =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid allowlist JSON"))?;
        list.keys()?;
        Ok(list)
    }
    fn keys(&self) -> Result<HashSet<String>> {
        if self.characters.len() > 500 {
            return Err(invalid("too many allowlist entries"));
        }
        let mut keys = HashSet::new();
        for c in &self.characters {
            if !keys.insert(identity(&c.name, &c.realm)?) {
                return Err(invalid("duplicate/ambiguous allowlist identity"));
            }
        }
        Ok(keys)
    }
    fn permits(&self, name: &str, realm: &str) -> bool {
        identity(name, realm).ok().is_some_and(|key| {
            self.characters
                .iter()
                .any(|c| identity(&c.name, &c.realm).ok().as_ref() == Some(&key))
        })
    }
}
// Deliberately conservative: ASCII folds only; Unicode letters match EXACTLY.
// Composed/decomposed lookalikes, case variants and connected realms never infer consent.
fn identity(name: &str, realm: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 64
        || realm.is_empty()
        || realm.len() > 128
        || !name.chars().all(char::is_alphabetic)
        || !realm
            .chars()
            .all(|c| c.is_alphabetic() || c.is_ascii_digit() || matches!(c, ' ' | '\'' | '-'))
    {
        return Err(invalid(
            "identity requires an explicit character name and realm",
        ));
    }
    let realm: String = realm
        .chars()
        .filter(|c| !matches!(c, ' ' | '\'' | '-'))
        .collect();
    if realm.is_empty() {
        return Err(invalid("realm is empty"));
    }
    Ok(format!(
        "{}@{}",
        name.to_ascii_lowercase(),
        realm.to_ascii_lowercase()
    ))
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    name: String,
    realm: String,
    key_level: i64,
    #[serde(rename = "challengeMapID")]
    challenge_map_id: i64,
    dungeon_name: String,
    observed_at: i64,
    reset_at: i64,
    source: String,
}
impl Snapshot {
    fn validate(&self, now: i64) -> Result<()> {
        identity(&self.name, &self.realm)?;
        if !(0..=100).contains(&self.key_level)
            || !(0..=100000).contains(&self.challenge_map_id)
            || ((self.key_level == 0) != (self.challenge_map_id == 0))
            || !matches!(self.source.as_str(), "OWN" | "PARTY" | "GUILD")
            || self.dungeon_name.is_empty()
            || self.dungeon_name.len() > 256
            || self.dungeon_name.chars().any(char::is_control)
            || self.observed_at < 1_600_000_000
            || self.observed_at > now + 300
            || self.reset_at <= self.observed_at
            || self.reset_at > self.observed_at + 8 * 86400
        {
            return Err(invalid("invalid snapshot fields/timestamps"));
        }
        Ok(())
    }
    fn freshness(&self, now: i64) -> &'static str {
        if now >= self.reset_at {
            "expired (weekly reset)"
        } else if now - self.observed_at > STALE_AFTER {
            "stale"
        } else {
            "fresh recorded snapshot"
        }
    }
}

/// Read-only Discord view over the same synced database used by the companion.
pub(crate) struct KeystoneService {
    database: BotDatabase,
    allow_path: PathBuf,
}
impl KeystoneService {
    pub(crate) fn initialize(
        database: Option<BotDatabase>,
    ) -> Result<Option<std::sync::Arc<Self>>> {
        let Some(database) = database else {
            return Ok(None);
        };
        let allow_path = default_allowlist_path();
        Allowlist::load(&allow_path)?;
        Ok(Some(std::sync::Arc::new(Self {
            database,
            allow_path,
        })))
    }
    pub(crate) async fn prepare_report(&self) -> Result<String> {
        let list = Allowlist::load(&self.allow_path)?;
        let now = Utc::now().timestamp();
        let mut records = active_keys(&self.database, &list, now).await?;
        // Consent may have changed while the cloud pull was in flight.
        let current = Allowlist::load(&self.allow_path)?;
        let now = Utc::now().timestamp();
        records.retain(|r| current.permits(&r.name, &r.realm) && now < r.reset_at);
        Ok(format_keys(&records, now))
    }
}
fn default_allowlist_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CHUNGUSCHILLERCORD_KEYSTONE_ALLOWLIST") {
        return PathBuf::from(path);
    }
    let runtime = PathBuf::from(DEFAULT_ALLOWLIST);
    if runtime.exists() {
        runtime
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_ALLOWLIST)
    }
}
async fn active_keys(database: &BotDatabase, list: &Allowlist, now: i64) -> Result<Vec<Snapshot>> {
    let keys = list.keys()?;
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let _guard = database.lock().await;
    database.pull().await?;
    let conn = database.connect().await?;
    let mut records = Vec::new();
    for key in keys {
        // Select latest BEFORE filtering positive/reset states: never resurrect an older key.
        let mut rows = conn.query("SELECT character_name, realm, key_level, challenge_map_id, dungeon_name, observed_at, reset_at, source FROM wow_keystone_snapshots WHERE identity_key = ? ORDER BY observed_at DESC, CASE source WHEN 'OWN' THEN 0 WHEN 'PARTY' THEN 1 ELSE 2 END LIMIT 1", (key.clone(),)).await?;
        if let Some(row) = rows.next().await? {
            let r = Snapshot {
                name: row.get(0)?,
                realm: row.get(1)?,
                key_level: row.get(2)?,
                challenge_map_id: row.get(3)?,
                dungeon_name: row.get(4)?,
                observed_at: row.get(5)?,
                reset_at: row.get(6)?,
                source: row.get(7)?,
            };
            r.validate(now)?;
            if identity(&r.name, &r.realm)? != key || !list.permits(&r.name, &r.realm) {
                return Err(invalid("database read boundary denied mismatched identity"));
            }
            if r.key_level > 0 && now < r.reset_at {
                records.push(r)
            }
        }
    }
    records.sort_by(|a, b| {
        b.key_level
            .cmp(&a.key_level)
            .then_with(|| a.dungeon_name.cmp(&b.dungeon_name))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.realm.cmp(&b.realm))
    });
    Ok(records)
}
const KEYS_FOOTER: &str = "Addon: every 60s + events; companion: every 10s. WoW saves on /reload, logout or clean exit; unflushed changes won't appear. Party/guild updates need a received broadcast. Stale = over 6h, availability unverified.";
fn discord_escape(text: &str) -> String {
    let mut escaped = String::new();
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '*' | '_' | '`' | '~' | '>' | '<' | '[' | ']' | '(' | ')' | '#' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}
fn format_keys(records: &[Snapshot], now: i64) -> String {
    if records.is_empty() {
        return format!("No active allowlisted keystone snapshots are available.\n\n{KEYS_FOOTER}");
    }
    let mut message = "**Recorded keystones**\n".to_string();
    let mut shown = 0;
    for r in records {
        let freshness = if now - r.observed_at > STALE_AFTER {
            "stale, unverified"
        } else {
            "fresh snapshot"
        };
        let line = format!(
            "**+{} {}** — {} · {} — recorded <t:{}:R> ({})\n",
            r.key_level,
            discord_escape(&r.dungeon_name),
            discord_escape(&r.name),
            discord_escape(&r.realm),
            r.observed_at,
            freshness
        );
        // Reserve room for the footer and the omitted count; never split a row or Unicode.
        if message.encode_utf16().count()
            + line.encode_utf16().count()
            + KEYS_FOOTER.encode_utf16().count()
            + 80
            > 2000
        {
            break;
        }
        message.push_str(&line);
        shown += 1;
    }
    if shown < records.len() {
        message.push_str(&format!(
            "… {} more keys omitted to fit Discord's message limit.\n",
            records.len() - shown
        ));
    }
    message.push('\n');
    message.push_str(KEYS_FOOTER);
    message
}

// Whitelisted Lua grammar: one assignment, tables, strings and integers ONLY.
// No calls, expressions, metatables, long strings, globals or executable code.
struct Lua<'a> {
    input: &'a [u8],
    pos: usize,
    nodes: usize,
}
impl<'a> Lua<'a> {
    fn ws(&mut self) {
        loop {
            while self
                .input
                .get(self.pos)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.pos += 1;
            }
            if self.input.get(self.pos..self.pos + 2) == Some(b"--") {
                while self.input.get(self.pos).is_some_and(|b| *b != b'\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }
    fn byte(&mut self, b: u8) -> Result<()> {
        self.ws();
        if self.input.get(self.pos) != Some(&b) {
            return Err(invalid("unsupported/malformed Lua data"));
        }
        self.pos += 1;
        Ok(())
    }
    fn word(&mut self) -> Result<String> {
        self.ws();
        let start = self.pos;
        while self
            .input
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
        {
            self.pos += 1;
        }
        if start == self.pos {
            return Err(invalid("expected a data key"));
        }
        Ok(String::from_utf8(self.input[start..self.pos].to_vec()).unwrap())
    }
    fn string(&mut self) -> Result<String> {
        self.ws();
        let quote = *self
            .input
            .get(self.pos)
            .ok_or_else(|| invalid("truncated string"))?;
        if quote != b'"' && quote != b'\'' {
            return Err(invalid("expected quoted string"));
        }
        self.pos += 1;
        let mut out = Vec::new();
        while let Some(&b) = self.input.get(self.pos) {
            self.pos += 1;
            if b == quote {
                return String::from_utf8(out).map_err(|_| invalid("invalid UTF-8"));
            }
            if b == b'\\' {
                let escape = *self
                    .input
                    .get(self.pos)
                    .ok_or_else(|| invalid("truncated escape"))?;
                self.pos += 1;
                match escape {
                    b'\\' | b'"' | b'\'' => out.push(escape),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'a' => out.push(7),
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'v' => out.push(11),
                    b'0'..=b'9' => {
                        let mut number = (escape - b'0') as u16;
                        for _ in 0..2 {
                            if let Some(&digit) = self.input.get(self.pos) {
                                if digit.is_ascii_digit() {
                                    number = number * 10 + (digit - b'0') as u16;
                                    self.pos += 1;
                                } else {
                                    break;
                                }
                            }
                        }
                        out.push(
                            u8::try_from(number).map_err(|_| invalid("invalid decimal escape"))?,
                        );
                    }
                    _ => return Err(invalid("unsupported string escape")),
                }
            } else {
                if b < 32 {
                    return Err(invalid("unescaped control in string"));
                }
                out.push(b);
            }
        }
        Err(invalid("unterminated string"))
    }
    fn value(&mut self, depth: usize) -> Result<serde_json::Value> {
        self.ws();
        self.nodes += 1;
        if depth > 12 || self.nodes > 50000 {
            return Err(invalid("Lua data complexity limit exceeded"));
        }
        match self.input.get(self.pos).copied() {
            Some(b'{') => {
                self.pos += 1;
                let mut table = serde_json::Map::new();
                loop {
                    self.ws();
                    if self.input.get(self.pos) == Some(&b'}') {
                        self.pos += 1;
                        break;
                    }
                    let key = if self.input.get(self.pos) == Some(&b'[') {
                        self.pos += 1;
                        let key = self.string()?;
                        self.byte(b']')?;
                        key
                    } else {
                        self.word()?
                    };
                    self.byte(b'=')?;
                    let value = self.value(depth + 1)?;
                    if table.insert(key, value).is_some() {
                        return Err(invalid("duplicate Lua table key"));
                    }
                    self.ws();
                    match self.input.get(self.pos) {
                        Some(b',') | Some(b';') => self.pos += 1,
                        Some(b'}') => {}
                        _ => return Err(invalid("malformed table separator")),
                    }
                }
                Ok(table.into())
            }
            Some(b'"') | Some(b'\'') => Ok(self.string()?.into()),
            Some(b'0'..=b'9') => {
                let value = self
                    .word()?
                    .parse::<i64>()
                    .map_err(|_| invalid("expected integer"))?;
                Ok(value.into())
            }
            _ => Err(invalid("executable/unsupported Lua rejected")),
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Export {
    schema_version: u32,
    #[serde(default)]
    last_flush_at: Option<i64>,
    records: BTreeMap<String, serde_json::Value>,
}
struct Parsed {
    allowed: Vec<Snapshot>,
    blocked: usize,
}
fn parse(input: &[u8], list: &Allowlist, now: i64) -> Result<Parsed> {
    if input.len() as u64 > MAX_BYTES {
        return Err(invalid("SavedVariables size limit exceeded"));
    }
    let mut lua = Lua {
        input,
        pos: 0,
        nodes: 0,
    };
    if lua.word()? != "CCCKeystonesDB" {
        return Err(invalid("unexpected SavedVariables global"));
    }
    lua.byte(b'=')?;
    let value = lua.value(0)?;
    lua.ws();
    if lua.pos != input.len() {
        return Err(invalid("trailing/executable Lua rejected"));
    }
    let export: Export =
        serde_json::from_value(value).map_err(|_| invalid("invalid export schema"))?;
    if export.schema_version != 1
        || export
            .last_flush_at
            .is_some_and(|time| time < 1_600_000_000 || time > now + 300)
    {
        return Err(invalid("unsupported schema or invalid flush timestamp"));
    }
    let mut parsed = Parsed {
        allowed: Vec::new(),
        blocked: 0,
    };
    for (key, value) in export.records {
        let record: Snapshot =
            serde_json::from_value(value).map_err(|_| invalid("invalid record schema"))?;
        record.validate(now)?;
        if identity(&record.name, &record.realm)? != key {
            return Err(invalid("record identity/key mismatch"));
        }
        if list.permits(&record.name, &record.realm) {
            parsed.allowed.push(record);
        } else {
            parsed.blocked += 1;
        }
    }
    Ok(parsed)
}

fn discover(root: &Path) -> Result<Vec<PathBuf>> {
    let account = root.join("WTF/Account");
    let mut paths = Vec::new();
    for entry in fs::read_dir(account)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let file = entry.path().join("SavedVariables/CCCKeyRecorder.lua");
            if file.exists() {
                paths.push(file);
            }
        }
    }
    paths.sort();
    Ok(paths)
}
fn read_export(path: &Path, root: &Path) -> Result<(Vec<u8>, i64)> {
    let root = root.canonicalize()?;
    let path = path.canonicalize()?;
    let relative = path
        .strip_prefix(root.join("WTF/Account"))
        .map_err(|_| invalid("SavedVariables path escapes the selected Retail installation"))?;
    let parts: Vec<_> = relative.components().collect();
    if parts.len() != 3
        || parts[1].as_os_str() != "SavedVariables"
        || parts[2].as_os_str() != "CCCKeyRecorder.lua"
    {
        return Err(invalid(
            "select account SavedVariables/CCCKeyRecorder.lua (not a backup or character file)",
        ));
    }
    let before = fs::metadata(&path)?;
    if !before.is_file() || before.len() > MAX_BYTES {
        return Err(invalid("invalid SavedVariables file/size"));
    }
    let modified = before.modified()?;
    let bytes = fs::read(&path)?;
    let after = fs::metadata(&path)?;
    if before.len() != after.len()
        || modified != after.modified()?
        || bytes.len() as u64 != before.len()
    {
        return Err(invalid(
            "file changed during read; retry after WoW finishes saving",
        ));
    }
    Ok((
        bytes,
        modified
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("invalid file timestamp"))?
            .as_secs() as i64,
    ))
}

async fn import(
    database: &BotDatabase,
    list: &Allowlist,
    records: &[Snapshot],
    file_modified: i64,
) -> Result<usize> {
    // Revalidate consent and all fields at the database boundary too.
    let now = Utc::now().timestamp();
    for record in records {
        record.validate(now)?;
        if !list.permits(&record.name, &record.realm) {
            return Err(invalid("database boundary denied non-allowlisted identity"));
        }
    }
    let _guard = database.lock().await;
    database.pull().await?;
    let conn = database.connect().await?;
    conn.execute("BEGIN IMMEDIATE", ()).await?;
    let result: Result<usize> = async {
        let mut inserted = 0;
        for r in records {
            let key = identity(&r.name, &r.realm)?;
            // Same identity/time/source must be identical, not silently replace history.
            let mut previous = conn.query("SELECT key_level, challenge_map_id, reset_at, dungeon_name FROM wow_keystone_snapshots WHERE identity_key = ? AND observed_at = ? AND source = ?", (key.clone(), r.observed_at, r.source.clone())).await?;
            if let Some(row) = previous.next().await? {
                if row.get::<i64>(0)? != r.key_level || row.get::<i64>(1)? != r.challenge_map_id || row.get::<i64>(2)? != r.reset_at || row.get::<String>(3)? != r.dungeon_name { return Err(invalid("conflicting repeat snapshot rejected")); }
                continue;
            }
            inserted += conn.execute("INSERT INTO wow_keystone_snapshots (character_name, realm, identity_key, observed_at, reset_at, key_level, challenge_map_id, dungeon_name, source, imported_at, file_modified_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (r.name.clone(), r.realm.clone(), key, r.observed_at, r.reset_at, r.key_level, r.challenge_map_id, r.dungeon_name.clone(), r.source.clone(), now, file_modified)).await? as usize;
        }
        Ok(inserted)
    }.await;
    match result {
        Ok(count) => {
            conn.execute("COMMIT", ()).await?;
            database.push().await?;
            Ok(count)
        }
        Err(err) => {
            let _ = conn.execute("ROLLBACK", ()).await;
            Err(err)
        }
    }
}
async fn show(database: &BotDatabase, list: &Allowlist) -> Result<()> {
    let _guard = database.lock().await;
    database.pull().await?;
    let conn = database.connect().await?;
    let now = Utc::now().timestamp();
    let mut count = 0;
    for c in &list.characters {
        let key = identity(&c.name, &c.realm)?;
        let mut rows = conn.query("SELECT character_name, realm, key_level, challenge_map_id, dungeon_name, observed_at, reset_at, source, imported_at, file_modified_at FROM wow_keystone_snapshots WHERE identity_key = ? ORDER BY observed_at DESC, CASE source WHEN 'OWN' THEN 0 WHEN 'PARTY' THEN 1 ELSE 2 END LIMIT 1", (key,)).await?;
        if let Some(row) = rows.next().await? {
            let r = Snapshot {
                name: row.get(0)?,
                realm: row.get(1)?,
                key_level: row.get(2)?,
                challenge_map_id: row.get(3)?,
                dungeon_name: row.get(4)?,
                observed_at: row.get(5)?,
                reset_at: row.get(6)?,
                source: row.get(7)?,
            };
            r.validate(now)?;
            println!(
                "{}",
                serde_json::json!({"snapshot": r, "freshness": r.freshness(now), "importedAt": row.get::<i64>(8)?, "fileModifiedAt": row.get::<i64>(9)?})
            );
            count += 1;
        }
    }
    println!(
        "Database latest rows for current allowlist: {count}. Saved file snapshots; not continuous live sync."
    );
    Ok(())
}

pub(crate) async fn run(args: &[String]) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("help");
    if command == "help" {
        println!(
            "Owner-only keystone companion: --keystones preview|import|watch|show|report|allowlist\nOptions: --retail PATH --saved-variables PATH --allowlist PATH\nallowlist: list|add|remove NAME REALM (regenerate addon Allowlist.lua via scripts/install-wow-addon.py)\npreview is offline. import/watch/show reuse existing TURSO_DATABASE_URL/TURSO_AUTH_TOKEN. No Discord starts."
        );
        return Ok(());
    }
    let mut retail = PathBuf::from(DEFAULT_RETAIL);
    let mut file = None;
    let mut allow_path = default_allowlist_path();
    let mut positional = Vec::new();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--retail" | "--saved-variables" | "--allowlist" => {
                let option = &args[i];
                i += 1;
                let value = args.get(i).ok_or_else(|| invalid("missing option value"))?;
                match option.as_str() {
                    "--retail" => retail = value.into(),
                    "--allowlist" => allow_path = value.into(),
                    _ => file = Some(PathBuf::from(value)),
                }
            }
            value if value.starts_with('-') => return Err(invalid("unknown option")),
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    let mut list = Allowlist::load(&allow_path)?;
    if command == "allowlist" {
        match positional.first().map(String::as_str).unwrap_or("list") {
            "list" if positional.len() <= 1 => {
                println!("{}", serde_json::to_string_pretty(&list).unwrap())
            }
            action @ ("add" | "remove") if positional.len() == 3 => {
                let c = Character {
                    name: positional[1].clone(),
                    realm: positional[2].clone(),
                };
                let key = identity(&c.name, &c.realm)?;
                list.characters.retain(|existing| {
                    identity(&existing.name, &existing.realm).ok().as_ref() != Some(&key)
                });
                if action == "add" {
                    list.characters.push(c);
                }
                list.keys()?;
                fs::write(
                    &allow_path,
                    format!("{}\n", serde_json::to_string_pretty(&list).unwrap()),
                )?;
                println!(
                    "Allowlist updated. Run the addon installer and safely reload WoW. Existing imported history is retained; removed identities are hidden and never imported again."
                );
            }
            _ => return Err(invalid("allowlist expects list OR add/remove NAME REALM")),
        }
        return Ok(());
    }
    if !positional.is_empty()
        || !matches!(command, "preview" | "import" | "watch" | "show" | "report")
    {
        return Err(invalid("unknown command/argument (see --keystones help)"));
    }
    if command == "report" {
        if list.characters.is_empty() {
            println!("{}", format_keys(&[], Utc::now().timestamp()));
            return Ok(());
        }
        let service = KeystoneService {
            database: BotDatabase::required_from_env().await?,
            allow_path,
        };
        println!("{}", service.prepare_report().await?);
        return Ok(());
    }
    if command == "show" {
        let db = BotDatabase::required_from_env().await?;
        return show(&db, &list).await;
    }
    if list.characters.is_empty() {
        println!("Allowlist empty: default deny; no database connection or upload.");
        return Ok(());
    }
    // Validate all files offline BEFORE opening the existing database.
    let mut database = None;
    loop {
        list = Allowlist::load(&allow_path)?; // Consent changes take effect on every polling pass.
        let paths = match &file {
            Some(path) => vec![path.clone()],
            None => discover(&retail)?,
        };
        if paths.is_empty() {
            println!(
                "No flushed CCCKeyRecorder.lua yet. Enable recorder, /ccckeys, then safely /reload or logout."
            );
        }
        let now = Utc::now().timestamp();
        let mut batches = Vec::new();
        for path in paths {
            let (bytes, modified) = read_export(&path, &retail)?;
            let parsed = parse(&bytes, &list, now)?;
            println!(
                "{}: allowed={}, blocked={}, fileModifiedAt={modified}",
                path.display(),
                parsed.allowed.len(),
                parsed.blocked
            );
            for r in &parsed.allowed {
                println!(
                    "{}",
                    serde_json::json!({"snapshot": r, "freshness": r.freshness(now)})
                );
            }
            batches.push((parsed.allowed, modified));
        }
        if command != "preview" && batches.iter().any(|(records, _)| !records.is_empty()) {
            if database.is_none() {
                database = Some(BotDatabase::required_from_env().await?);
            }
            for (records, modified) in &batches {
                println!(
                    "Inserted {} new snapshots into existing Turso database.",
                    import(database.as_ref().unwrap(), &list, records, *modified).await?
                );
            }
            show(database.as_ref().unwrap(), &list).await?;
        }
        if command != "watch" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn list() -> Allowlist {
        Allowlist {
            characters: vec![Character {
                name: "Fixtureowner".into(),
                realm: "Zul'jin".into(),
            }],
        }
    }
    fn snapshot(now: i64) -> Snapshot {
        Snapshot {
            name: "Fixtureowner".into(),
            realm: "Zul'jin".into(),
            key_level: 16,
            challenge_map_id: 503,
            dungeon_name: "Fixture Dungeon".into(),
            observed_at: now,
            reset_at: now + 3600,
            source: "OWN".into(),
        }
    }
    fn lua_export(r: &Snapshot, key: &str) -> String {
        format!(
            r#"CCCKeystonesDB = {{ ["schemaVersion"] = 1, ["records"] = {{ ["{key}"] = {{ name="{}", realm="{}", keyLevel={}, challengeMapID={}, dungeonName="{}", observedAt={}, resetAt={}, source="{}", }}, }}, }}"#,
            r.name,
            r.realm,
            r.key_level,
            r.challenge_map_id,
            r.dungeon_name,
            r.observed_at,
            r.reset_at,
            r.source
        )
    }
    #[test]
    fn safe_parser_consent_and_identity() {
        let now = Utc::now().timestamp();
        let r = snapshot(now);
        let input = lua_export(&r, "fixtureowner@zuljin");
        assert_eq!(
            parse(input.as_bytes(), &list(), now).unwrap().allowed,
            vec![r]
        );
        let denied = parse(input.as_bytes(), &Allowlist { characters: vec![] }, now).unwrap();
        assert_eq!(denied.blocked, 1);
        assert!(denied.allowed.is_empty());
        assert_eq!(
            identity("FIXTUREOWNER", "Zuljin").unwrap(),
            identity("Fixtureowner", "Zul'jin").unwrap()
        );
        assert_ne!(
            identity("Fixtureowner", "Illidan").unwrap(),
            identity("Fixtureowner", "Zul'jin").unwrap()
        );
        assert_ne!(
            identity("Éclair", "Zuljin").unwrap(),
            identity("éclair", "Zuljin").unwrap()
        );
        assert!(identity("E\u{301}clair", "Zuljin").is_err()); // No fuzzy consent for decomposed accents.
        assert!(identity("Fixtureowner", "").is_err());
        assert!(identity("Fixtureowner-Zuljin", "Zuljin").is_err());
        assert!(
            parse(
                lua_export(&snapshot(now), "fixtureowner@illidan").as_bytes(),
                &list(),
                now
            )
            .is_err()
        );
    }
    #[test]
    fn malformed_executable_and_limits() {
        let now = Utc::now().timestamp();
        let good = lua_export(&snapshot(now), "fixtureowner@zuljin");
        for bad in [
            format!("{good}\nos.execute('touch /tmp/never')"),
            good.replace("keyLevel=16", "keyLevel=tonumber(16)"),
            good.replace("schemaVersion\"] = 1", "schemaVersion\"] = 2"),
            good[..good.len() - 1].into(),
            good.replace("keyLevel=16", "keyLevel=0"),
            good.replace("source=\"OWN\"", "source=\"WHISPER\""),
            good.replace(
                "name=\"Fixtureowner\"",
                "name=\"Fixtureowner\",name=\"Fixtureowner\"",
            ),
        ] {
            assert!(
                parse(bad.as_bytes(), &list(), now).is_err(),
                "must reject {bad}"
            );
        }
        assert!(parse(&vec![b' '; MAX_BYTES as usize + 1], &list(), now).is_err());
        let mut lua = Lua {
            input: b"\"\\195\\137clair\"",
            pos: 0,
            nodes: 0,
        };
        assert_eq!(lua.string().unwrap(), "Éclair");
        let mut future = snapshot(now + 301);
        assert!(future.validate(now).is_err());
        future.observed_at = now;
        future.reset_at = now;
        assert!(future.validate(now).is_err());
    }
    #[test]
    fn freshness_and_no_key() {
        let now = Utc::now().timestamp();
        let mut r = snapshot(now);
        assert_eq!(r.freshness(now), "fresh recorded snapshot");
        r.reset_at = now + 86400;
        assert_eq!(r.freshness(now + STALE_AFTER + 1), "stale");
        assert_eq!(r.freshness(r.reset_at), "expired (weekly reset)");
        r.key_level = 0;
        r.challenge_map_id = 0;
        r.dungeon_name = "No keystone".into();
        r.validate(now).unwrap();
    }
    #[test]
    fn path_selection_and_backup_rejection() {
        let dir = std::env::temp_dir().join(format!("ccc-path-{}", std::process::id()));
        let account = dir.join("_retail_/WTF/Account/FIXTURE/SavedVariables");
        fs::create_dir_all(&account).unwrap();
        let root = dir.join("_retail_");
        let file = account.join("CCCKeyRecorder.lua");
        fs::write(&file, b"CCCKeystonesDB = {}").unwrap();
        assert_eq!(discover(&root).unwrap(), vec![file.clone()]);
        assert!(read_export(&file, &root).is_ok());
        let backup = account.join("CCCKeyRecorder.lua.bak");
        fs::write(&backup, b"ignore").unwrap();
        assert!(read_export(&backup, &root).is_err());
        let outside = dir.join("CCCKeyRecorder.lua");
        fs::write(&outside, b"ignore").unwrap();
        assert!(read_export(&outside, &root).is_err());
        #[cfg(unix)]
        {
            let link = account.join("escape");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(read_export(&link, &root).is_err());
        }
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn existing_client_database_repeat_update_and_deny() {
        let dir = std::env::temp_dir().join(format!("ccc-keys-db-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = BotDatabase::new_local(&dir.join("fixture.db"))
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        let r = snapshot(now);
        assert_eq!(import(&db, &list(), &[r.clone()], now).await.unwrap(), 1);
        assert_eq!(import(&db, &list(), &[r.clone()], now).await.unwrap(), 0);
        let mut updated = r.clone();
        updated.observed_at += 1;
        updated.key_level = 0;
        updated.challenge_map_id = 0;
        updated.dungeon_name = "No keystone".into();
        assert_eq!(import(&db, &list(), &[updated], now).await.unwrap(), 1);
        assert!(
            import(&db, &Allowlist { characters: vec![] }, &[r.clone()], now)
                .await
                .is_err()
        );
        let mut conflict = r.clone();
        conflict.key_level = 17;
        assert!(import(&db, &list(), &[conflict], now).await.is_err());
        let conn = db.connect().await.unwrap();
        let mut rows = conn
            .query("SELECT COUNT(*) FROM wow_keystone_snapshots", ())
            .await
            .unwrap();
        assert_eq!(
            rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
            2
        );
        let mut latest=conn.query("SELECT key_level, challenge_map_id FROM wow_keystone_snapshots ORDER BY observed_at DESC LIMIT 1",()).await.unwrap();
        let row = latest.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), 0);
        assert_eq!(row.get::<i64>(1).unwrap(), 0);
        drop(latest);
        drop(rows);
        drop(conn);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn keys_format_freshness_escaping_and_message_limit() {
        let now = Utc::now().timestamp();
        let empty = format_keys(&[], now);
        assert!(empty.contains("No active allowlisted"));
        assert!(empty.contains("60s") && empty.contains("10s") && empty.contains("/reload"));
        let mut fresh = snapshot(now);
        fresh.name = "Éclair".into();
        fresh.dungeon_name = "Dungeon **name** <@123> [click](https://example.test)".into();
        let mut stale = snapshot(now - STALE_AFTER - 1);
        stale.reset_at = now + 3600;
        let message = format_keys(&[fresh.clone(), stale], now);
        assert!(message.contains("Éclair"));
        assert!(message.contains("Zul'jin"));
        assert!(message.contains(&format!("<t:{now}:R> (fresh snapshot)")));
        assert!(message.contains("stale, unverified"));
        assert!(message.contains(r"\*\*name\*\*"));
        assert!(!message.contains("<@123>"));
        let many = format_keys(&vec![fresh; 500], now);
        assert!(many.encode_utf16().count() <= 2000);
        assert!(many.contains("more keys omitted"));
        assert!(many.ends_with(KEYS_FOOTER));
    }

    #[tokio::test]
    async fn keys_latest_state_consent_reset_and_source_precedence() {
        let dir = std::env::temp_dir().join(format!("ccc-keys-command-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = BotDatabase::new_local(&dir.join("fixture.db"))
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        let old = snapshot(now - 10);
        let mut party = snapshot(now - 2);
        party.source = "PARTY".into();
        party.key_level = 14;
        let mut own = party.clone();
        own.source = "OWN".into();
        own.key_level = 18;
        import(&db, &list(), &[old, party, own.clone()], now)
            .await
            .unwrap();
        // Synthetic non-allowlisted row must never leak into the public report.
        let foreign_list = Allowlist {
            characters: vec![Character {
                name: "Fixturepug".into(),
                realm: "Illidan".into(),
            }],
        };
        let mut foreign = snapshot(now);
        foreign.name = "Fixturepug".into();
        foreign.realm = "Illidan".into();
        import(&db, &foreign_list, &[foreign], now).await.unwrap();
        assert_eq!(active_keys(&db, &list(), now).await.unwrap(), vec![own]);
        let mut no_key = snapshot(now - 1);
        no_key.key_level = 0;
        no_key.challenge_map_id = 0;
        no_key.dungeon_name = "No keystone".into();
        import(&db, &list(), &[no_key], now).await.unwrap();
        assert!(active_keys(&db, &list(), now).await.unwrap().is_empty());
        let mut expired = snapshot(now);
        expired.reset_at = now + 1;
        import(&db, &list(), &[expired], now).await.unwrap();
        assert!(active_keys(&db, &list(), now + 1).await.unwrap().is_empty());
        let allow_path = dir.join("allowlist.json");
        fs::write(&allow_path, serde_json::to_vec(&list()).unwrap()).unwrap();
        let service = KeystoneService {
            database: db.clone(),
            allow_path: allow_path.clone(),
        };
        assert!(
            !service
                .prepare_report()
                .await
                .unwrap()
                .contains("Fixturepug")
        );
        fs::write(&allow_path, br#"{"characters":[]}"#).unwrap();
        assert!(
            service
                .prepare_report()
                .await
                .unwrap()
                .contains("No active allowlisted")
        );
        fs::write(&allow_path, b"invalid JSON").unwrap();
        assert!(service.prepare_report().await.is_err());
        drop(service);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn keys_reject_corrupt_identity_and_database_errors() {
        let dir = std::env::temp_dir().join(format!("ccc-keys-error-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = BotDatabase::new_local(&dir.join("fixture.db"))
            .await
            .unwrap();
        let now = Utc::now().timestamp();
        import(&db, &list(), &[snapshot(now)], now).await.unwrap();
        let conn = db.connect().await.unwrap();
        conn.execute("UPDATE wow_keystone_snapshots SET realm = 'Illidan'", ())
            .await
            .unwrap();
        assert!(active_keys(&db, &list(), now).await.is_err());
        conn.execute("DROP TABLE wow_keystone_snapshots", ())
            .await
            .unwrap();
        assert!(active_keys(&db, &list(), now).await.is_err());
        // Default deny avoids even a failed database access.
        assert!(
            active_keys(&db, &Allowlist { characters: vec![] }, now)
                .await
                .unwrap()
                .is_empty()
        );
        drop(conn);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}
