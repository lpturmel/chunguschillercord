-- Standalone deterministic stubs, NOT live group evidence.
local handlers, callback, timers = {}, nil, {}
local now, level, map, ownName, ownRealm = 1800000000, 16, 503, "Fixtureowner", "Zul'jin"
local requests = 0
function CreateFrame() return { RegisterEvent = function() end, SetScript = function(_, _, handler) handlers.event = handler end } end
function GetServerTime() return now end
function UnitFullName() return ownName, ownRealm end
function GetRealmName() return ownRealm end
function GetNormalizedRealmName() return ownRealm:gsub("[ '%-]", "") end
function IsInGroup() return true end
function IsInGuild() return true end
C_DateAndTime = { GetSecondsUntilWeeklyReset = function() return 3600 end }
C_ChallengeMode = { GetMapUIInfo = function() return "Fixture Dungeon" end }
C_MythicPlus = { GetOwnedKeystoneLevel = function() return level end, GetOwnedKeystoneChallengeMapID = function() return map end, RequestMapInfo = function() end }
C_Timer = { After = function(_, f) timers[#timers+1] = f end, NewTicker = function(_, f) timers[#timers+1] = f end }
local lib = { Register = function(_, f) callback = f end, Unregister = function() end, Request = function() requests = requests+1 end }
LibStub = function() return lib end
SlashCmdList = {}
CCCKeystonesDB = { schemaVersion=1, records = { ['pug@zuljin'] = {name='Pug', realm="Zul'jin"} } }
CCCKeyRecorderAllowlist = { {name='Fixtureowner', realm="Zul'jin"}, {name='Friend', realm='Area 52'}, {name='Éclair', realm="Zul'jin"} }
assert(loadfile('addon/CCCKeyRecorder/Recorder.lua'))('CCCKeyRecorder')
handlers.event(nil, 'ADDON_LOADED', 'CCCKeyRecorder')
assert(CCCKeystonesDB.records['pug@zuljin'] == nil)
handlers.event(nil, 'PLAYER_LOGIN')
for _, timer in ipairs(timers) do timer() end
assert(requests == 2)
local r = CCCKeystonesDB.records['fixtureowner@zuljin']
assert(r and r.keyLevel == 16 and r.challengeMapID == 503 and r.source == 'OWN')
callback(18,503,0,'Pug', 'PARTY')
assert(CCCKeystonesDB.records['pug@zuljin'] == nil)
callback(18,503,0,'Friend', 'PARTY') -- Bare name means local realm, NEVER guess Area 52.
assert(CCCKeystonesDB.records['friend@area52'] == nil)
callback(18,503,0,'Friend-Area52', 'PARTY')
assert(CCCKeystonesDB.records['friend@area52'].keyLevel == 18)
callback(18,503,0,'Fixtureowner-Illidan', 'GUILD')
assert(CCCKeystonesDB.records['fixtureowner@illidan'] == nil)
callback(19,503,0,'Éclair', 'GUILD')
assert(CCCKeystonesDB.records['Éclair@zuljin'].keyLevel == 19)
callback(19,503,0,'éclair', 'GUILD')
assert(CCCKeystonesDB.records['éclair@zuljin'] == nil)
now = now+60; level, map = 0, 0
handlers.event(nil, 'PLAYER_LOGOUT')
r = CCCKeystonesDB.records['fixtureowner@zuljin']
assert(r.keyLevel == 0 and r.challengeMapID == 0 and r.observedAt == now)
assert(CCCKeystonesDB.lastFlushAt == now)
CCCKeyRecorderAllowlist = {}
handlers.event(nil, 'PLAYER_LOGOUT')
assert(next(CCCKeystonesDB.records) == nil)
print('PASS addon own/party/guild, deny pugs, realm ambiguity, Unicode exact, update/no-key, allowlist removal')
