# State persistence

## Goal

Persist pseudoclient state across bridge restarts so that channel memberships,
idle timeout tracking, and offline timeout tracking survive a restart without
resetting.

Without persistence, every restart causes all pseudoclients to lose their
lazily-joined channel memberships and resets all idle/offline timeout clocks.
For an operator running with 2-week channel idle timeouts and 30-day offline
timeouts, a restart wipes all that tracking state.

## What is persisted

Only state that **cannot be reconstructed from live sources** is persisted:

| Field | Type | Why |
|-------|------|-----|
| `channels` | `Vec<String>` | Lazy join state — Discord has no concept of which IRC channels a user joined |
| `last_active` | `u64` | Global activity timestamp for offline timeout tracking |
| `channel_last_active` | `HashMap<String, u64>` | Per-channel activity for channel idle timeout |
| `went_offline_at` | `Option<u64>` | Offline transition timestamp — without this, a 30-day timer resets on restart |

## What is NOT persisted

These are reconstructed from live data on each startup:

- **`uid`** — per-session; UID generator assigns fresh UIDs during burst
- **`nick`** — re-resolved during burst against the live network nick namespace
- **`username`, `display_name`** — provided fresh by Discord `MemberSnapshot`
- **`presence`** — provided fresh by Discord `MemberSnapshot`/`PresenceUpdated`
- **`needs_reintroduce`** — per-session KILL tracking; irrelevant across restarts
- **`known_nicks`** (external nicks) — populated from the IRC burst
- **`IrcState`, `DiscordState`** caches — rebuilt from live events

## Configuration

A new optional field in `[pseudoclients]`:

```toml
[pseudoclients]
state_file = "/var/lib/disirc/state.json"
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `state_file` | `Option<String>` | `None` | Path to the JSON state file. If absent, state persistence is disabled. |

When `state_file` is not set, the bridge behaves exactly as before — all
pseudoclient state is ephemeral.

## Store format

JSON file via `serde_json`. Rationale:

- The data is a flat map of a few hundred entries — no need for a database.
- `serde_json` is already a dev-dependency (promoted to a regular dependency).
- JSON is human-debuggable and a natural fit for data serialization.
- TOML (already a dep) was considered but is awkward for data files: channel
  names containing `#` require quoted keys, nested `HashMap` values are verbose.
- SQLite was considered but adds a C dependency and schema migration complexity
  for a single-table key-value store that is bulk-read once and periodically
  bulk-written.

### File structure

```json
{
  "version": 1,
  "pseudoclients": {
    "123456789": {
      "channels": ["#general", "#dev"],
      "last_active": 1700000000,
      "channel_last_active": {
        "#general": 1700000000,
        "#dev": 1699900000
      },
      "went_offline_at": null
    }
  }
}
```

Keys in the `pseudoclients` map are Discord user ID strings (JSON doesn't
support integer keys). A `version` field allows future format migration.

## Flush strategy

State is written periodically, not on every mutation. This keeps I/O off the
hot path (message relay).

- **Periodic save**: on the existing 60-second idle tick in `run_bridge`.
- **Clean shutdown save**: on SIGTERM/SIGHUP before exit.
- **Dirty flag**: only write if state has changed since the last save.

### Atomic writes

To prevent corruption on crash mid-write:

1. Write to `<state_file>.tmp`.
2. `fsync` the temp file.
3. Rename `<state_file>.tmp` → `<state_file>`.

On Unix, `rename(2)` is atomic. This guarantees the state file is always
either the old complete version or the new complete version.

### Worst-case data loss

On crash (no clean shutdown), up to 60 seconds of activity timestamps may be
lost. For timeouts measured in weeks and months, this is negligible. Channel
membership changes (lazy joins, idle PARTs) are also captured within 60
seconds.

## Startup merge

On startup, persisted state is loaded into a **seed map** on `BridgeState`
(`HashMap<u64, PersistedPseudoclient>`). This seed map is separate from
`PseudoclientManager` — it holds only the persisted fields, not UIDs or nicks.

When `apply_discord_event` processes `MemberSnapshot` and introduces each
pseudoclient via `introduce_pseudoclient`, it checks the seed map:

- **Seed exists for this user**: apply persisted `channels`,
  `last_active`, `channel_last_active`, and `went_offline_at` to the
  newly-created `PseudoclientState`. Fresh `username`, `display_name`,
  and `presence` from the snapshot take precedence.
- **No seed**: introduce as normal (empty channels, current timestamp).

The seed map is consumed entry-by-entry as users are introduced. Entries for
users who left the guild since last run are never consumed and are silently
discarded.

### Channel validation on restore

Persisted channel memberships are filtered against the current bridge
configuration. If a bridge entry was removed since last run, the channel is
dropped from the restored membership list (no stale JOINs for channels the
bridge no longer manages).

### Bot user

The bot pseudoclient eagerly joins all bridged channels regardless of persisted
state (it must be present in every channel for S2S routing). Persisted bot
state is ignored.

## Burst with persisted channels

During `produce_burst_commands`, pseudoclients with restored channel lists emit
SJOIN commands for those channels, just as they would for channels joined in
the current session. No special handling is needed — the burst already walks
`PseudoclientState.channels`.

## Module placement

State persistence logic lives in a new module `src/persist.rs`:

- `PersistedState` — top-level serializable struct (version + pseudoclients map)
- `PersistedPseudoclient` — per-user serializable data
- `load_state(path) -> Result<PersistedState>` — read and deserialize
- `save_state(path, &BridgeState) -> Result<()>` — serialize and atomic write
- `snapshot_from_pm(&PseudoclientManager) -> PersistedState` — extract persistable state

The `run_bridge` loop calls `save_state` on the idle tick and on shutdown.
`BridgeState::new` accepts an optional `PersistedState` to populate the seed
map.

## Error handling

- **Load failure** (file missing, corrupt JSON, wrong version): log a warning
  and start with empty state. Do not abort startup — the bridge can always
  start fresh.
- **Save failure** (permission denied, disk full): log a warning per attempt.
  Do not crash — the bridge continues operating with in-memory state.
- **Version mismatch**: if the `version` field is not `1`, log a warning and
  ignore the file. Future versions can add migration logic.
