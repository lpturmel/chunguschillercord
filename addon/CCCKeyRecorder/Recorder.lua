local addonName = ...
local listener, lib = {}, nil
local frame = CreateFrame("Frame")
local ready = false

-- ASCII case folding preserves Unicode exactly. No fuzzy accent, connected-realm,
-- transliteration, or realm-less matching. Display realm spaces/apostrophes/hyphens
-- are removed just as the normalized realm token is used by WoW senders.
local function canonical(name, realm)
    if type(name) ~= "string" or type(realm) ~= "string" then return end
    if name == "" or realm == "" or name:find("[%c%s%-%p%d]") then return end
    if realm:find("[%c]") then return end
    local n = name:gsub("[A-Z]", string.lower)
    local r = realm:gsub("[ '%-]", ""):gsub("[A-Z]", string.lower)
    if r == "" then return end
    return n .. "@" .. r
end
local function allowed(name, realm)
    local key = canonical(name, realm)
    if not key then return end
    for _, entry in ipairs(CCCKeyRecorderAllowlist or {}) do
        if key == canonical(entry.name, entry.realm) then return key end
    end
end
local function prune()
    for key, record in pairs(CCCKeystonesDB.records) do
        if type(record) ~= "table" or allowed(record.name, record.realm) ~= key then
            CCCKeystonesDB.records[key] = nil
        end
    end
end
local function record(name, realm, level, map, source)
    local key = allowed(name, realm)
    if not key then return end -- Never persist non-whitelisted identities.
    if type(level) ~= "number" or type(map) ~= "number" or level < 0 or level > 100
        or map < 0 or level % 1 ~= 0 or map % 1 ~= 0
        or ((level == 0) ~= (map == 0)) then return end
    local now = GetServerTime()
    local seconds = C_DateAndTime and C_DateAndTime.GetSecondsUntilWeeklyReset()
    if type(seconds) ~= "number" or seconds <= 0 then return end
    local dungeon = map > 0 and C_ChallengeMode.GetMapUIInfo(map) or "No keystone"
    CCCKeystonesDB.records[key] = {
        name = name, realm = realm, keyLevel = level, challengeMapID = map,
        dungeonName = dungeon or ("Challenge map " .. map),
        observedAt = now, resetAt = now + math.floor(seconds), source = source,
    }
end
local function own()
    if not ready then return end
    local name, realm = UnitFullName("player")
    realm = realm or GetRealmName()
    -- Match BigWigs/LibKeystone's normal Retail own-key API behavior.
    local level = C_MythicPlus.GetOwnedKeystoneLevel() or 0
    local map = C_MythicPlus.GetOwnedKeystoneChallengeMapID() or 0
    record(name, realm, level, map, "OWN")
end
local function register()
    local current = LibStub and LibStub("LibKeystone", true)
    if current and current ~= lib then
        if lib then lib.Unregister(listener) end
        lib = current
        lib.Register(listener, function(level, map, _, playerName, channel)
            if not ready or (channel ~= "PARTY" and channel ~= "GUILD") then return end
            -- LibKeystone Ambiguate(..., 'none') removes the realm for SAME-realm
            -- senders. Bare names therefore refer only to this player's realm.
            local name, realm = playerName:match("^([^%-]+)%-(.+)$")
            if not name then name, realm = playerName, GetNormalizedRealmName() end
            local ownName, ownRealm = UnitFullName("player")
            if canonical(name, realm) == canonical(ownName, ownRealm or GetRealmName()) then
                own() -- Prefer direct own API; never relabel a broadcast as own.
            else
                record(name, realm, level, map, channel)
            end
        end)
    end
end
local function request()
    register()
    own()
    if lib then
        if IsInGroup() then lib.Request("PARTY") end
        if IsInGuild() then lib.Request("GUILD") end
    end
end
frame:SetScript("OnEvent", function(_, event, name)
    if event == "ADDON_LOADED" then
        if name == addonName then
            if type(CCCKeystonesDB) ~= "table" or CCCKeystonesDB.schemaVersion ~= 1 then
                CCCKeystonesDB = { schemaVersion = 1, records = {} }
            end
            if type(CCCKeystonesDB.records) ~= "table" then CCCKeystonesDB.records = {} end
            prune()
        end
        register()
    elseif event == "PLAYER_LOGIN" then
        ready = true
        C_MythicPlus.RequestMapInfo()
        C_Timer.After(5, request)
        C_Timer.NewTicker(60, own)
    elseif event == "PLAYER_LOGOUT" then
        own()
        prune()
        CCCKeystonesDB.lastFlushAt = GetServerTime()
    elseif event == "GROUP_ROSTER_UPDATE" or event == "PLAYER_GUILD_UPDATE" then
        C_Timer.After(2, request)
    else
        C_Timer.After(1, own)
    end
end)
for _, event in ipairs({"ADDON_LOADED", "PLAYER_LOGIN", "PLAYER_LOGOUT", "BAG_UPDATE_DELAYED", "CHALLENGE_MODE_COMPLETED", "CHALLENGE_MODE_MAPS_UPDATE", "GROUP_ROSTER_UPDATE", "PLAYER_GUILD_UPDATE"}) do
    frame:RegisterEvent(event)
end
SLASH_CCCKEYS1 = "/ccckeys"
SlashCmdList.CCCKEYS = function(command)
    if command == "refresh" then
        request()
    else
        local name, realm = UnitFullName("player")
        realm = realm or GetRealmName()
        local key = allowed(name, realm)
        local saved = key and CCCKeystonesDB.records[key]
        print("CCC Keystone Recorder: " .. name .. "-" .. realm .. (key and " allowed" or " BLOCKED (not allowlisted)"))
        print("Own API: " .. (C_MythicPlus.GetOwnedKeystoneLevel() or 0) .. " / map " .. (C_MythicPlus.GetOwnedKeystoneChallengeMapID() or 0))
        if saved then print("Recorded: " .. saved.dungeonName .. " +" .. saved.keyLevel .. ", observed " .. saved.observedAt) end
        print("LibKeystone: " .. (lib and "subscribed" or "missing; enable BigWigs") .. ". /ccckeys refresh requests keys.")
        print("SavedVariables reach the companion after a safe /reload, logout, or clean exit.")
    end
end
