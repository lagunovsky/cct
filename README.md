# cct

**Search everything you and Claude Code ever did.**

You've had hundreds of sessions with Claude Code. Somewhere in there is the fix
for the exact error you're staring at, the reason that config looks the way it
does, the command that finally made the build pass. `/resume` shows you titles.
`cct` finds the line.

```sh
cct "connection reset"                 # this project's sessions, best match first
cct -g "stripe webhook" "signature"    # every project; a session must hold both
cct blame src/auth/session.ts          # sessions that edited or discussed this file
cct search -s 499d80ae --since 2h "lock"   # every match inside one long session
cct show 499d80ae @495579478           # read the conversation around a match
cct                                    # recent sessions here, with their last prompts
```

## Why it's good

- **Searches what Claude actually saw** — your prompts, its answers, tool calls,
  command output, subagent logs, saved tool results. Not just session titles.
- **Fast on a big history.** A Tantivy index, updated incrementally before each
  search: 20 GB of logs answer in a fraction of a second.
- **Finds the place, not just the session.** `-s` lists every match in one
  session by time; `cct show` opens the conversation right there.
- **`cct blame <file>`** — which sessions changed a file (even through `sed` or
  a python script), which only talked about it.
- **Your agent can use it too.** `--json` output and a three-line CLAUDE.md
  snippet (below) let Claude look up its own past work instead of guessing.
- **Keeps history around.** Claude Code deletes transcripts after 30 days by
  default; `cct` warns about it, archives old sessions into zstd (4–8× smaller)
  and keeps them searchable.
- **Fixes `/resume` after you move a project.** `cct rename` and `cct transfer`
  carry sessions to the new path.
- **Ignores the noise.** System prompts, CLAUDE.md copies, base64 blobs and
  thousands of identical `claude -p` pipeline runs stay out of your results
  (`--headless` brings those back).

## Install

```sh
cargo install --git https://github.com/lagunovsky/cct
```

Needs a stable Rust toolchain; macOS or Linux. The first search builds the
index in `~/.claude/search-index/` — a few minutes on a large history, then
incremental. On macOS, `cct daemon install` keeps it fresh in the background.

## Teach Claude to use it

Add this to `~/.claude/CLAUDE.md`:

```markdown
## Session history
- `cct search --json "query"` searches past Claude Code sessions in this project (`-g` for all projects); several arguments must all match within one session
- `cct blame <path>` — sessions that discussed or edited a file
- To find a spot inside one session: `cct search -s <id> --since "2026-10-25 18:00" "query"`, then `cct show <id> @<location>`
```

Now "how did we fix this last time?" gets an answer from your actual history.

## Docs

- [Search](docs/search.md) — queries, ranking, filters, searching inside a session, JSON output
- [Blame](docs/blame.md) — which sessions worked on a file
- [Moving projects](docs/projects.md) — `rename` and `transfer`
- [Archive and retention](docs/archive.md) — `archive`, `unarchive`, the daemon, `cleanup-period`
- [Command reference](docs/commands.md)
- [How the index works](docs/internals.md) — what gets indexed, layout, updates, merging
- [Development](docs/development.md)

## License

MIT
