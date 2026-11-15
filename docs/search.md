# Search

```sh
cct "рефакторинг"                # search the current project's sessions
cct search "рефакторинг"         # the same; a bare argument implies `search`
cct search -g "API error"        # across all projects
cct search -p ../other "TODO"    # another project
cct search "auth" "timeout"      # a session must contain both strings
cct search -n 50 -c 200 "trace"  # 50 sessions, 200 chars of context per snippet
cct search --sort time "auth"    # newest first instead of best match first
```

## What is searched

Every session of the project: the main log, its subagent logs and its saved
tool results (`.txt`, `.json`, `.jsonl`, `.md`, `.log`, `.csv`, `.tsv`, `.html`,
`.xml`), including archived sessions. Session boilerplate — system prompt,
CLAUDE.md and memory copies, tool listings, base64 blobs — is not indexed; see
[internals](internals.md#what-gets-indexed).

By default the search covers the project of the current directory; `-g`
searches everything, `-p <path>` another project. Each search first applies an
incremental index update (skipped when nothing changed; `--no-update` turns it
off).

A bare first argument that is neither a flag nor a subcommand is a search, so
`cct ветка` runs `cct search ветка`. The flip side: a mistyped command
(`cct reindx`) becomes a search.

## Queries

- **One argument is a phrase.** `"could refute this"` matches those words in
  that order. Case doesn't matter.
- **Several arguments are AND at session level.** Each string may match
  anywhere in the session — main log, subagent, tool result — and a session is
  listed only when every string matched and showed a snippet.
- **`-f` / `--fuzzy`** treats the words of a string independently and shows a
  snippet per word, for when the phrase never appears on one line.
- **Substrings and identifiers.** Queries that look technical — mixed case,
  acronyms, digits, `_-:.#@`, or non-ASCII of 3+ characters — also go through a
  trigram index, so `APIError` finds `TypeAPIErrorHandler` and `давлен` finds
  `давление`. Queries with `/`, `\` or `~` also search file paths. Pin one
  backend with `--mode word|trigram|path|all`.
- **Long tokens** — keys, hashes, base64 — are found by their full value.
- **Slash commands** are searchable as typed: `/research` finds the sessions
  that ran it, and snippets show `/research <args>` instead of the raw XML.

## Result order

Results are ranked by relevance, not date. A session scores by the number of
lines that matched, weighted by where they matched (main log 1.0, subagent
0.7, tool results and hooks 0.5) and by how literal the match was (word 1.0,
trigram 0.7, path 0.4), passed through `ln(1+x)` so a sprawling session can't
win on volume alone. A match in your own prompt counts ×1.3. The score is
damped by age with a 180-day half-life: recency breaks ties between comparable
matches, but a clearly better old session still wins — which is the point of
asking "how did we solve this".

`--sort time` gives newest first regardless of match quality.

## Headless sessions

Sessions started headless — `claude -p` and the Agent SDK, which Claude Code
marks with `"entrypoint":"sdk-cli"` — are indexed but left out of `search`,
`blame` and `recent`. Pipelines start them by the thousand from one prompt
template and would crowd out the sessions a person actually had. `--headless`
includes them. Their subagent logs and tool results follow the session.

## Finding a place inside a session

A long session can match hundreds of times, and five snippets per session say
little. Search it alone and read around the match:

```sh
cct search -s 499d80ae --since "2026-10-25 18:00" --until 2026-10-25 "lock"
cct show 499d80ae @495579478           # the conversation around that match
cct show 499d80ae --time "2026-10-25 19:46" -C 10
cct show 499d80ae                      # the end of the session
```

`-s <id>` (a full id or unique prefix, any project, headless included) lists
every match in that session oldest first; `-n` then counts snippets. Each
snippet carries the `@location` of its line: `@<offset>` in the main log,
`@subagents/<file>:<offset>` in a subagent log. Plain searches print locations
with `-v`.

`--since` / `--until` take a date (`2026-10-25`, which `--until` reads as the
whole day), a time (`"2026-10-25 18:00"`) or an age (`30m`, `2h`, `3d`), and
work in any search.

`cct show` prints prompts, answers, tool calls and their results around the
point — `-C` records on each side (default 6), long records cut unless
`--full`. Without a location it opens the end of the session; `--time` opens
the first record at or after that moment.

## Snippets

Each session shows up to 5 snippets: matches in prompts and answers when there
are any, otherwise in tool output, otherwise in subagent logs and hooks. `-v`
shows all of them together, plus file paths and `@locations`. `-c N` sets
the context around each match (default 120 characters). Sessions renamed with
`/rename` show their name next to the id.

## JSON output

`--json` prints one JSON object per session on stdout and nothing else —
progress and warnings go to stderr:

```json
{"session_id": "…", "title": "…", "project": "~/Work/app", "archived": false,
 "last_activity": "2026-10-25T19:47:04+02:00", "prompts": 66, "tools": 96,
 "bytes": 2600000, "score": 2.31,
 "snippets": [{"tag": "answer", "time_ms": 1792953998000, "text": "…",
               "at": "@495579478", "file": "/Users/…/499d80ae….jsonl", "offset": 495579478}]}
```

`-v` widens snippets and adds `files`; with `-s` every snippet up to `-n` is
returned, plus `matches` — the total in the window. `score` is the relevance
before age damping, comparable within one search only.
