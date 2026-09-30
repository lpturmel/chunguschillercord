#!/usr/bin/env python3
"""User-run single-command guild upsert. Does not delete unrelated commands."""
import argparse
import getpass
import json
import os
import sys
import urllib.error
import urllib.request


def snowflake(value):
    if not value or not value.isascii() or not value.isdigit() or not 0 < int(value) < 2**64:
        raise argparse.ArgumentTypeError("use a positive Discord application/server ID")
    return str(int(value))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--application-id", type=snowflake, default=os.getenv("DISCORD_APPLICATION_ID"))
    parser.add_argument("--guild-id", type=snowflake, default=os.getenv("DISCORD_GUILD_ID"))
    parser.add_argument("--dry-run", action="store_true", help="show URL/payload without credentials or network")
    args = parser.parse_args()
    if not args.application_id or not args.guild_id:
        parser.error("provide --application-id and --guild-id (or their DISCORD_* environment variables)")
    url = f"https://discord.com/api/v10/applications/{args.application_id}/guilds/{args.guild_id}/commands"
    payload = {"name": "keys", "description": "List available allowlisted WoW keystone snapshots and their freshness", "type": 1}
    if args.dry_run:
        print("POST " + url)
        print(json.dumps(payload))
        print("Upserts /keys only; preserves other commands. No network request made.")
        return 0
    token = os.getenv("DISCORD_BOT_TOKEN", "").strip()
    if not token:
        if not sys.stdin.isatty():
            parser.error("set DISCORD_BOT_TOKEN through your existing secure environment or run in an interactive terminal")
        token = getpass.getpass("Existing Discord bot token (hidden): ").strip()
    if not token:
        parser.error("an existing Discord bot token is required")
    request = urllib.request.Request(url, data=json.dumps(payload).encode(), method="POST", headers={
        "Authorization": "Bot " + token, "Content-Type": "application/json",
        "User-Agent": "chunguschillercord/0.1 manual keys registration",
    })
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        print(f"Discord registration failed (HTTP {error.code}); check the existing bot token, application/server IDs and app installation.", file=sys.stderr)
        return 1
    except (urllib.error.URLError, TimeoutError, ValueError):
        print("Discord registration could not complete; check connectivity and try again.", file=sys.stderr)
        return 1
    if result.get("name") != "keys" or str(result.get("guild_id")) != args.guild_id or str(result.get("application_id")) != args.application_id:
        print("Discord returned an unexpected registration result; verify /keys in the intended application/server.", file=sys.stderr)
        return 1
    print(f"Registered /keys in server {args.guild_id} (command {result.get('id', 'unknown')}). Other commands were preserved.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
