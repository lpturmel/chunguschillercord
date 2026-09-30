# chunguschillercord

Discord backend with an owner-installed WoW Retail keystone recorder and local
Rust companion. `/keys` reads the existing Turso database and lists positive,
non-expired snapshots for explicitly allowlisted character and realm pairs.
The Rust Turso client is stable 0.8.1. See [setup and consent](docs/wow-keystones.md).

No roster is shipped: private runtime configuration is ignored, the example
allowlist is empty, and the addon denies collection until explicit consent is set.
Existing League/Valorant commands remain available through the signed interaction
endpoint. `GET /health` provides readiness. The keystone CLI exits before any
Discord worker or command registration starts.

```sh
cargo build --locked
cargo test --locked
lua scripts/test-wow-addon.lua
```

Copy the example configs to their private runtime filenames before use. Do not
commit credentials, player rosters or SavedVariables exports. Fly deployment uses
the existing app and volume; startup Discord registration is disabled by default
in `fly.toml`. Command registration is a separate user-run step.

The Valorant worker also persists rank snapshots in the shared database and reports
ranked-match wins, losses, draws and rating change over each scheduled interval.
Migration 3 adds its history table. Startup test posts remain disabled in Fly.
