# TODO — spec/06-pseudoclients

Status: **Implemented**

- [x] Nick sanitization (character replacement, digit prefix, truncation to 30 chars)
- [x] Nick collision fallback chain (`_` ×3, 8 hex digits of Discord ID, UID-derived guaranteed fallback)
- [x] UID generation (SID + 6 unique alphanumeric chars, stable per Discord user ID for session)
- [x] `PseudoclientState` struct and in-memory state maps (`discord_id → state`, `nick → id`, `uid → id`)
- [x] Introduction message generation (UID line + SJOIN line)
- [x] Quit/Part message generation
- [x] SVSNICK handling (apply forced nick change, update state)
- [x] Runtime channel add/remove (SJOIN existing pseudoclients to new channel; PART/QUIT on removal)
