# Archive and retention

## Keep your history

Claude Code deletes session transcripts after 30 days unless
`cleanupPeriodDays` in `~/.claude/settings.json` says otherwise — searchable
history quietly shrinks to a month. `cct reindex` warns when retention is under
90 days.

```sh
cct cleanup-period              # show the current value
cct cleanup-period 3650         # keep transcripts for ten years
cct cleanup-period --no-warn    # silence the warning (--warn to restore it)
```

## Archive

Old sessions can move out of `~/.claude/projects` into `~/.claude/archive`,
zstd-compressed (4–8× smaller on real logs) in a mirror of the projects layout.
They stay in the index: search finds them, snippets are decompressed on the
fly, results and `cct recent` mark them `[archived]`.

```sh
cct archive                            # sessions idle for 90+ days, with a prompt
cct archive --older-than 6m -p .       # one project, a different age
cct archive --headless-older-than 10d  # only headless (claude -p) sessions
cct archive --dry-run                  # list what would move
cct unarchive 499d80ae                 # bring one back for claude --resume
```

Ages are written as `90d`, `3m`, `1y` or a number of days. `--older-than` and
`--headless-older-than` can be combined: headless sessions then go by their own,
usually much shorter age.

`cct unarchive` restores a session with a fresh mtime, so Claude Code's
age-based cleanup doesn't reap it straight away; `--keep-mtime` keeps the
original at your own risk.

`cct rename` and `cct transfer` skip archived sessions; unarchive them first.

## Background updates

On macOS a launchd job keeps the index fresh every 30 minutes while on AC
power, so searches start from a warm index:

```sh
cct daemon install
cct daemon uninstall
```

It runs at background priority. Archiving from the daemon is opt-in and
stored in `~/.config/cct/config`:

```sh
cct archive --auto 120d            # archive sessions idle for 120+ days
cct archive --auto-headless 10d    # headless ones after 10 days
cct archive --auto off             # stop (--auto-headless off: follow --auto)
```

On other platforms `cct daemon install` fails; run `cct update` (or
`cct daemon update`) from cron or a systemd timer instead.
