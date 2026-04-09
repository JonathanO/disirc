# IRC Oper Commands TODO

## Tasks

- [ ] Task 1: Expose `is_oper` in `S2SEvent::UserIntroduced` from UID umodes
- [ ] Task 2: Parse user MODE commands, add `S2SEvent::UserModeChanged`
- [ ] Task 3: Track oper UIDs in `IrcState` (insert/remove on introduce/mode/quit/linkdown)
- [ ] Task 4: Command handler skeleton — intercept bot PRIVMSG from opers, parse and reply
- [ ] Task 5: `status` and `reload` commands (read-only)
- [ ] Task 6: Config write-back — `save_config` with atomic write
- [ ] Task 7: `bridge list` command
- [ ] Task 8: `bridge add` and `bridge remove` commands
- [ ] Task 9: `bridge set-webhook` and `bridge clear-webhook` commands
- [ ] Task 10: Mutation testing and cleanup
