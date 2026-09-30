#!/usr/bin/env python3
"""Install this owner's addon only; never edit SavedVariables or other addons."""
import argparse
import datetime
import json
from pathlib import Path
import shutil

repo = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser()
parser.add_argument('--retail', type=Path, default=Path('/Applications/World of Warcraft/_retail_'))
parser.add_argument('--allowlist', type=Path, default=repo / 'config/wow-keystones.json')
parser.add_argument('--check', action='store_true', help='Read-only verification of installed files')
args = parser.parse_args()
config = json.loads(args.allowlist.read_text())
if set(config) != {'characters'} or len(config['characters']) > 500:
    raise SystemExit('Invalid allowlist schema')
seen = set()
entries = []
for character in config['characters']:
    if set(character) != {'name', 'realm'}:
        raise SystemExit('Require both name and realm')
    name, realm = character['name'], character['realm']
    if not name or len(name.encode()) > 64 or not name.isalpha() or not realm or len(realm.encode()) > 128 or not all(c.isalpha() or ('0' <= c <= '9') or c in " '-" for c in realm):
        raise SystemExit('Invalid character/realm')
    # Match ASCII-only folding in the addon and companion, preserving Unicode.
    fold = lambda s: s.translate(str.maketrans('ABCDEFGHIJKLMNOPQRSTUVWXYZ', 'abcdefghijklmnopqrstuvwxyz'))
    identity = fold(name) + '@' + fold(realm.translate(str.maketrans('', '', " '-")))
    if identity in seen or identity.endswith('@'):
        raise SystemExit('Duplicate/ambiguous identity')
    seen.add(identity)
    entries.append('    { name = ' + json.dumps(name, ensure_ascii=False) + ', realm = ' + json.dumps(realm, ensure_ascii=False) + ' },')
allow_lua = '-- Generated from the explicit owner allowlist. Default deny; never auto-add rosters.\nCCCKeyRecorderAllowlist = {\n' + '\n'.join(entries) + '\n}\n'
retail = args.retail.resolve(strict=True)
if retail.name != '_retail_':
    raise SystemExit('Select the actual _retail_ installation')
addons = retail / 'Interface/AddOns'
if not addons.is_dir() or addons.is_symlink():
    raise SystemExit('Expected Retail Interface/AddOns directory')
target = addons / 'CCCKeyRecorder'
if target.is_symlink():
    raise SystemExit('Refusing symlink addon destination')
contents = {name: (repo / 'addon/CCCKeyRecorder' / name).read_bytes() for name in ('CCCKeyRecorder.toc', 'Recorder.lua')}
contents['Allowlist.lua'] = allow_lua.encode()
if args.check:
    for name, content in contents.items():
        installed = target / name
        if installed.is_symlink() or not installed.is_file() or installed.read_bytes() != content:
            raise SystemExit('Installed addon differs: ' + name)
    print('Installed recorder and explicit allowlist match (' + str(len(entries)) + ' identities).')
else:
    if target.exists():
        backup = repo / '.wow-addon-backups' / datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S%fZ')
        backup.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(target, backup, symlinks=True)
        print('Existing recorder backed up:', backup)
    target.mkdir(exist_ok=True)
    for name, content in contents.items():
        dest = target / name
        if dest.is_symlink():
            raise SystemExit('Refusing symlink addon file')
        dest.write_bytes(content)
    print('Installed:', target)
    print('Allowed identities:', len(entries), '(all others denied). Safely /reload or log in to load.')
