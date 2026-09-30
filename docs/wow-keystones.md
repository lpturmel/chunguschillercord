# WoW Retail keystone recorder, companion and /keys

Only the owner installs the addon and companion. Friends whose keys arrive through
BigWigs/LibKeystone need no additional installation. Own keys are sampled every
60 seconds and after relevant events; received party/guild broadcasts are recorded.
The companion checks SavedVariables every 10 seconds. WoW saves them only after
a safe `/reload`, logout or clean exit, so unflushed changes will not appear.
Friends must be online and broadcasting; this cannot fetch offline or unseen keys.

## Private configuration and installation

From the repository root, create ignored runtime files from the safe examples:

```sh
cp config/league-valorant-rank.example.ron config/league-valorant-rank.ron
cp config/wow-keystones.example.json config/wow-keystones.json
cargo build --locked
./target/debug/chunguschillercord --keystones allowlist add 'Exactname' 'Exact Realm'
python3 scripts/install-wow-addon.py
python3 scripts/install-wow-addon.py --check
```

Replace the fictional rank settings only if using the existing rank workers.
Every keystone entry requires exact character AND actual realm. No party or guild
roster automatically grants consent. ASCII case is ignored; realm spaces,
apostrophes and hyphens normalize. Unicode letters remain exact: no accent folding,
connected-realm inference or fuzzy match. An empty allowlist denies everything.
The installer generates the installed Allowlist.lua, backs up this addon and never
edits another addon or SavedVariables. Load the new addon/config when safe in WoW.

The addon filters before persistence. The companion safely parses the limited Lua
data grammar without executing Lua, validates fields/timestamps and rechecks
consent at the database write boundary. Backups, escaping symlinks and files outside
Retail account SavedVariables are rejected. Existing ignored `.env` is loaded in
place; use established TURSO_DATABASE_URL/TURSO_AUTH_TOKEN, never paste credentials
into chat. There is no alternate database service or credential provisioning.
The default local cache is the existing client's /tmp/chunguschillercord.db;
CHUNGUSCHILLERCORD_DATABASE_PATH selects the established persistent bot cache.

## Local application

```sh
./target/debug/chunguschillercord --keystones preview
./target/debug/chunguschillercord --keystones import
./target/debug/chunguschillercord --keystones show
./target/debug/chunguschillercord --keystones report
./target/debug/chunguschillercord --keystones watch
```

`preview` is offline; `import` writes once through the existing Turso client;
`show` reads latest allowed rows; `report` previews the exact `/keys` message
without starting Discord; `watch` imports flushed files every 10 seconds. Stop
watch with Ctrl-C or launch `scripts/Keystone Companion.command` in Terminal.
Use --retail PATH, --saved-variables PATH or --allowlist PATH for explicit paths.
Only actual Retail account SavedVariables/CCCKeyRecorder.lua is accepted.

Migration 4 creates wow_keystone_snapshots using the existing migration tracker.
Rows are retained as history, unchanged imports are idempotent and conflicting
identity/time/source repeats roll back. Latest selection orders by observation,
with OWN winning source ties. A newer no-key observation suppresses an older
positive key. Weekly reset expires the snapshot. Older than six hours is a stale
heuristic, not proof of disappearance; stale pre-reset keys are marked unverified.

```sh
./target/debug/chunguschillercord --keystones allowlist list
./target/debug/chunguschillercord --keystones allowlist remove 'Exactname' 'Exact Realm'
python3 scripts/install-wow-addon.py
```

Reinstall and safely reload WoW after consent changes. Companion passes and
Discord commands reload the allowlist; removal hides retained historical rows
without deleting data. Bot and companion must use the same private config.
CHUNGUSCHILLERCORD_KEYSTONE_ALLOWLIST can point both to a managed shared file;
changing a local config alone does not change an already deployed bot.

## Discord

`/keys` replies with dungeon, level, character, realm and relative observation
time, ordered by level. It selects the latest record BEFORE filtering no-key/reset
states, excludes removed/unlisted/unseen characters and marks stale keys. It
explains the 60s/10s sampling and safe flush requirements in every report.
Text is escaped, mentions are disabled and large lists include an omitted count.
A deferred reply permits up to 20 seconds for cloud work; pull/validation failures
return a generic error instead of presenting old cache data. The existing
60-second per-user/per-command cooldown applies.

Use existing Discord application ID, public key, bot token and DISCORD_GUILD_ID
in the authorized backend environment. `/keys` is disabled without a guild ID;
signed requests must contain a member of that same server. DMs/other servers are
denied. A linked League/Valorant player is not required for `/keys`.
Keep the established HTTPS interactions URL ending in /discord/interactions.
The read-only --discord-registration-info backend CLI returns nonsecret app/server
IDs for the bot's existing configured channel; it never prints its token.

Fly uses the existing app/volume, matching Bookworm image stages and stable Turso
0.8.1. Its config disables automatic startup registration with
DISCORD_REGISTER_COMMANDS_ON_STARTUP=false; other launches retain existing default
behavior unless disabled. After verifying backend readiness, register ONLY /keys:

```sh
python3 scripts/register-keys-command.py --application-id APP_ID --guild-id SERVER_ID
```

The helper uses the existing DISCORD_BOT_TOKEN environment or a hidden interactive
prompt. --dry-run prints URL/payload without credentials or network. It uses the
[official guild-command POST upsert](https://docs.discord.com/developers/interactions/application-commands#create-guild-application-command),
preserving unrelated commands instead of bulk-overwriting them. No secret is stored
or printed. Live registration and actual invocation are separate from local tests.

## Verification

Run cargo test --locked, lua scripts/test-wow-addon.lua and the installer --check.
Tests use synthetic temporary fixtures, covering malformed/executable data, sizes,
unsafe paths, consent, wrong/ambiguous realms, exact Unicode, no-key/reset/stale,
updates/repeats/rollback, latest source precedence, dynamic consent, corrupt rows,
format limits and signed-request guild denial. No synthetic keys are uploaded.

For owner end-to-end QA, launch Retail through Battle.net Play, stay outside combat
and active instances, compare /ccckeys with the own API and /key, safely flush,
then preview/import/show/report. Verify observed dungeon/level, reset and freshness.
A real no-key state is valid; never fabricate a positive key. Private live exports,
rosters and detailed QA evidence are kept outside Git publication.
