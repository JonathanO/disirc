# TODO — spec/06-pseudoclients

Status: **Pending**

- [ ] Nick sanitization (character replacement, digit prefix, truncation to 30 chars)
- [ ] Nick collision fallback chain (`_` ×3, 8 hex digits of Discord ID, UID-derived guaranteed fallback)
- [ ] UID generation (SID + 6 unique alphanumeric chars, stable per Discord user ID for session)
- [ ] `PseudoclientState` struct and in-memory state maps (`discord_id → state`, `nick → id`, `uid → id`)
- [ ] Introduction message generation (UID line + SJOIN line)
- [ ] Quit/Part message generation
- [ ] SVSNICK handling (apply forced nick change, update state)
- [ ] Runtime channel add/remove (SJOIN existing pseudoclients to new channel; PART/QUIT on removal)
