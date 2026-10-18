# cct

Claude Code Tools — fast full-text search across Claude Code session logs, plus
utilities to rename projects and transfer sessions when project directories change.

The search backend is [Tantivy](https://github.com/quickwit-oss/tantivy): three
physical indexes (word, trigram, path) sit under `~/.claude/search-index/`,
share a single TSV catalog of file metadata, and support incremental updates
that only re-read the bytes that actually changed.

The corpus comes from the standard Claude Code layout:

```
~/.claude/projects/<encoded-project>/<session>.jsonl
~/.claude/projects/<encoded-project>/<session>/subagents/*.jsonl
~/.claude/projects/<encoded-project>/<session>/tool-results/*.{txt,json,jsonl,md,log,csv,tsv,html,htm,xml}
```

PDFs and other binary tool-results are deliberately not indexed.

Neither are the `attachment` records Claude Code writes into every session —
the system prompt snapshot, skill and tool listings, CLAUDE.md and auto-memory,
output style, environment, model, date. Since September 2026 they are about
2 GB of a 10.8 GB corpus and identical across sessions, so a word from your
instructions or memory used to match thousands of sessions at once. The
attachments that record what happened in a session stay indexed: prompts queued
while the agent worked, subagent `structured_output`, edited files, diagnostics,
todos, hook errors, plans. The exclusion list is `BOILERPLATE_ATTACHMENTS` in
`src/text.rs`; snippets skip those lines too, so an index built before the
change stops showing them even before `cct reindex`.

## Install

```sh
# clone and build a release binary
cargo build --release
./target/release/cct --help

# or install into ~/.cargo/bin
cargo install --path .
```

## Quick start

```sh
cct                              # recent sessions in cwd
cct recent -n 50                 # explicit, more results
cct search "марат"               # full-text search in cwd's project
cct "марат"                      # same — a bare argument implies `search`
cct search -g "API error"        # global search across all projects
cct search -p ../other "TODO"    # search a different project
cct search -n 50 -c 200 "trace"
cct search "auth" "timeout"      # AND: session must contain all strings
cct search --sort time "auth"    # newest first instead of best match first
cct blame src/index/search.rs    # sessions that discussed this file
cct reindex                      # rebuild the index from scratch
cct update                       # incremental update (also runs implicitly on search)
cct compact                      # collapse Tantivy segments to a single one
cct daemon install               # 30 min background updates on AC power
cct daemon uninstall
```

## Index layout

```
~/.claude/search-index/
  word/         SimpleTokenizer + LowerCaser → natural-language queries, short tokens like "АД"
  trigram/      NgramTokenizer(3,3) + LowerCaser → APIError, traceback substrings
  path/         trigram over file paths
  catalog.tsv   per-file row: file_id, kind, size, mtime_ms, indexed_offset, next_chunk_id, prompts, tools, display_bytes, title, path
  manifest.txt  source dir, enabled indexes, writer config used to build it
```

The `word` index truncates tokens to 32 characters. On real logs everything
above that length is base64 blobs, hashes and ids — 30% of the vocabulary and
68% of its bytes, 88% of them seen exactly once — so storing only their prefix
shrinks the term dictionary substantially without making them unfindable: a
query for the full string is truncated the same way and matches the same term,
and the snippet pass then verifies the whole string against the file, so two
blobs sharing a prefix cannot pass as each other. The cap counts characters, not
bytes, so Cyrillic words are not cut at half the length.

Dropping oversized tokens instead was the first attempt — it shrinks `word` by
~70% rather than by a prefix — on the theory that `trigram` could answer such
queries as substrings. On the real index it cannot: a 512 KB chunk contains
nearly every trigram, so AND-ing them barely narrows the candidate set and the
one chunk holding the string never survives the candidate limit. Searching for a
key you actually have is worth more than the extra megabytes.

Word postings carry term frequencies (`WithFreqs`); the ngram indexes do not.
Frequencies cost ~9% of the `word` index and are what let a chunk that mentions
the query twenty times outrank one that mentions it once.

Together, truncation and a fresh full merge took a real 10.4 GB corpus from a
6.8 GB index to 2.2 GB (`word` 5.1 GB → 1.7 GB), and searches on it run in
60–140 ms instead of 0.4–1.2 s. Old indexes keep working; `cct reindex` is what
applies the change.

`file_id` is stable across updates so deletes / re-adds use a single
`delete_term` per physical index.

Each Tantivy document also stores an exact `project_id` (the encoded project
directory under `~/.claude/projects`) so cwd / `--project` searches can filter
inside Tantivy instead of collecting global candidates first. Schema/catalog
changes are not backward-compatible; rebuild old indexes with `cct reindex`.

Each Tantivy document is one **content chunk** (~512 KB of complete jsonl
lines), not a whole file, so snippets can be regenerated by reading just the
chunk's byte range from disk.

## Search behaviour

A bare first argument that is neither a flag nor a known subcommand is treated
as an implicit `search` query, so `cct ветка` runs `cct search ветка`. The
trade-off is that a mistyped command (`cct reindx`) becomes a search rather
than an error.

`cct search` opens an incremental update before querying; if nothing changed,
the writer cycle is skipped entirely. Pass `--no-update` to suppress it.

Project-scoped searches (`cwd` default or `--project`) add an exact
`project_id` filter inside Tantivy.

The query analyzer is the same SimpleTokenizer as the index, so token-level
matches work consistently. `cct search` always queries the **word** index and
adds:

- **trigram** when the query looks technical: mixed case, all-uppercase
  acronym (`API`, `HTTP`), digit, `_-:.#@`, or non-ASCII length ≥ 3 (so
  `давление` adds trigram while a 2-char `АД` stays word-only).
- **path** when the query contains `/`, `\`, or `~`.

You can pin to a specific backend with `--mode word|trigram|path|all`.

Multiple query arguments are AND-ed at the session level: each string may
match anywhere in the session (main log, subagent log, or tool result), and
only sessions containing all of them are returned. Index selection above is
per string. Within a single string, tokens must co-occur in nearby context.

`--json` switches to machine-readable output for agents and scripts: one JSON
object per session on stdout (id, title, project, archived, timestamps,
prompt/tool/byte stats, snippets with tags), no colors or progress text;
progress and warnings go to stderr. Content matches the human output — `-v`
widens snippets to agent/hook matches and adds matched file paths, so shallow
searches stay cheap by default.

## Result order

Results are ranked by relevance, not by date. A session's score is the number
of lines that actually matched — weighted by where they matched (`session` 1.0,
`subagent` 0.7, tool results and hooks 0.5) and by which index found them
(`word` 1.0, `trigram` 0.7, `path` 0.4) — passed through `ln(1+x)` so a long
session cannot win on volume alone. A match in your own prompt gets ×1.3, and
the whole score is damped by age with a 180-day half life, which leaves recency
as a tiebreaker between comparable matches rather than the answer itself.

Counting matched lines rather than trusting Tantivy's BM25 is deliberate: a
document here is a 512 KB chunk, and BM25's length normalization cancels out
almost exactly the term frequency it is supposed to reward. BM25 still decides
which chunks come back as candidates.

`--sort time` restores the old behaviour — newest session first, regardless of
match quality. `--json` reports the pre-damping score per session.

Multi-word queries are AND-ed and matched as a phrase on one line by default.
Pass `-f`/`--fuzzy` to treat tokens independently and emit per-token snippets
even when the full phrase never appears on a single line.

Slash-command invocations (`<command-name>/research</command-name>` blocks in
the logs) are indexed and rendered as plain `/research <args>`, so searching
for `/research` finds the sessions that ran it.

Sessions named via `/rename` (`{"type":"custom-title",…}` records) show that
name next to the session id in both `recent` and `search` output; the last
rename wins. The title is cached in the catalog so search never re-reads the
jsonl for it.

Snippets come from the chunk byte range stored on each Tantivy doc, then
re-extracted as JSON values when possible. The default context window is `120`
characters around the match (`-c N` to override).

## Update strategy

| change kind                       | action                                                                  |
|-----------------------------------|-------------------------------------------------------------------------|
| new file                          | full add with new `file_id`                                             |
| `.jsonl` grew                     | read `[indexed_offset, last_complete_newline)` only — partial trailing line is left until next update |
| non-jsonl rewrite, shrink, mtime change | delete by `file_id`, re-add fully                                  |
| file removed                      | delete by `file_id`, drop from catalog                                  |
| unchanged                         | no read, no writer touch                                                |

A single `cct update` is enough for normal use. The `daemon` subcommand wires
that up to launchd at 30-minute intervals so most searches run on an already
warm index.

After a search, an update, a daemon run or an archive pass, `cct` tidies the
index. The delta is merged once a sub-index reaches 8 segments. The main index
is merged once it reaches 16 segments, or once more than 20% of its documents
are deleted: the small segments, plus any segment that is more than 30% dead,
fold into one, while the 4 largest healthy segments are left alone. That keeps
each pass cheap — it touches what accumulated since the last one, not the
gigabytes written by the reindex.

Before this, nothing merged the main index except an explicit `cct compact`.
Every flush commits a segment into main, and a flush re-adds a grown session
whole while only marking its previous copy deleted. Ten weeks of the daemon
flushing every 30 minutes left 3,500 segments per sub-index and 48% of all
documents dead; with a term dictionary per segment, `.term` files alone took
5.7 of the `word` index's 7 GB.

## Merge policy / memory

| command   | merge policy default | memory default                                                       |
|-----------|----------------------|----------------------------------------------------------------------|
| `reindex` | `log` (Tantivy default) | physical RAM / 6, clamped 768 MB … 8192 MB, fallback 5048 MB      |
| `update`  | `none` during the update, then the routine tidy above | 768 MB total, split per enabled index |
| `compact` | `none` + explicit merge to `--target-segments` (default 1) | 768 MB                                                        |

All three honor `--memory-mb`, `--worker-threads`, `--merge-threads`, and
`--only word,trigram,path`.

## Blame

`cct blame <path>` answers the other direction: not "where was this word
discussed" but "which sessions worked on this file, and what did they say".

```sh
cct blame src/index/search.rs      # sessions in the current project
cct blame -g Cargo.toml            # across all projects
cct blame --json src/cli.rs        # one object per session, plus "mention": edited|qualified|bare
```

The index knows nothing about which files a session touched — transcripts
mention paths as ordinary text — so blame searches for the basename and then
verifies every hit, in three tiers:

1. **edited** — the session changed the file: an `Edit`/`Write`/`MultiEdit`
   call on it, or an `edited_text_file` record. The latter is what Claude Code
   writes when a file the session has seen changes on disk, and it is the only
   trace a Bash, sed or python edit leaves — 45% of all such records are the
   session's own Bash edits. These sessions print an `edited <path>` line, and
   the record shows up as an `[edited]` snippet.
2. **qualified** — the session wrote `src/index/search.rs` or a longer tail.
3. **bare** — only `search.rs`; a name inside a longer word (`research.rsync`)
   is dropped.

Within each tier the normal relevance order applies. An `edited_text_file` for
CLAUDE.md or an auto-memory file is not indexed: Claude Code broadcasts it into
every session open at the time (20 sessions for one MEMORY.md change), and the
session that made the edit shows it through its own `Edit`/`Write` call.

An existing path is resolved against the filesystem, so `cct blame ./cli.rs`
and `cct blame src/cli.rs` behave the same. A path that no longer exists still
works — it is matched as typed, which is the point when the file was deleted
months ago.

## Rename / transfer

If a project's directory was renamed or moved, `/resume` in Claude Code stops
finding past sessions because they live under
`~/.claude/projects/<encoded-path>/` and the embedded `cwd` field still points
at the old path.

`cct rename` physically moves the project directory and migrates all of its
sessions in one step:

```sh
cct rename ../new-name                    # old = cwd
cct rename /old/abs /new/abs              # explicit
cct rename --no-move-dir . ../new         # dir already moved on disk; only fix sessions
cct rename --dry-run . ../new             # preview only
```

This moves the on-disk project dir, renames
`~/.claude/projects/<encoded-old>` → `<encoded-new>`, and rewrites every
occurrence of the old absolute path inside `.jsonl` files (including `cwd`).
Pass `--no-move-dir` when the project directory was already moved by hand and
only the session logs need their `cwd` rewired. The next `cct update` (or any
`cct search`) reconciles the index by deleting the old `file_id`s and re-adding
the relocated files.

`cct transfer` moves individual sessions from one project into another:

```sh
cct transfer ../other                     # interactive: pick sessions from cwd
cct transfer ../other abc123 def456       # by full id or unique prefix
cct transfer ../other abc123,def456       # comma-separated also accepted
cct transfer --from /old/proj ../other abc  # source other than cwd
cct transfer --dry-run ../other abc123    # preview
cct transfer -y ../other abc123           # skip confirmation prompt
```

Selection in the picker accepts space-toggling, `a`/`A` for select-all/none,
`/` to filter, and Enter to confirm.

## Commands

| Command | Description |
|---------|-------------|
| `cct search <query>` | Full-text search; flags: `-n`, `-c`, `-v`, `-g`, `-p`, `-f`, `--json`, `--mode`, `--sort`, `--no-update` |
| `cct blame <path>` | Which sessions discussed a file, and what they said; flags: `-n`, `-c`, `-v`, `-g`, `-p`, `--json`, `--no-update` |
| `cct recent` | Recent sessions in cwd; flag: `-n` |
| `cct reindex` | Rebuild the Tantivy index from scratch |
| `cct update` | Incremental update (auto on each search; manual when scripted) |
| `cct compact` | Merge Tantivy segments to `--target-segments` (default 1) |
| `cct rename [old] <new>` | Move project dir + migrate sessions + rewrite `cwd`; flags: `--no-move-dir`, `--dry-run` |
| `cct transfer <target> [ids...]` | Transfer specific sessions between projects; flags: `--from`, `-y`, `--dry-run` |
| `cct daemon install` | Install launchd job for background indexing every 30 min on AC power |
| `cct daemon uninstall` | Remove launchd job |
| `cct daemon update` | Background update entry point used by launchd |
| `cct cleanup-period [days]` | Show or set Claude Code's transcript retention (`cleanupPeriodDays` in `~/.claude/settings.json`); flags: `--no-warn`, `--warn` |
| `cct archive` | Move cold sessions into `~/.claude/archive` (zstd); flags: `--older-than` (default 90d), `-p`, `--dry-run`, `-y`, `--auto <age\|off>` |
| `cct unarchive <ids...>` | Restore archived sessions back into `~/.claude/projects`; flag: `--keep-mtime` |

## Archiving

`cct archive` moves sessions whose last activity is older than `--older-than`
(default 90 days) into `~/.claude/archive`, mirroring the projects layout with
each file zstd-compressed (~4-8x smaller on real logs). The search index keeps
its documents: archived sessions still come up in `cct search`, snippets are
served by on-the-fly decompression, and results are marked `[archived]`.
Restoring with `cct unarchive <id>` brings the session back for
`claude --resume`; the restored file gets a fresh mtime so Claude Code's
age-based cleanup doesn't immediately reap it (pass `--keep-mtime` to
preserve the original timestamp instead — at your own cleanup risk).

Background archiving is opt-in: `cct archive --auto 90d` makes the launchd
daemon archive cold sessions after each background index update
(`cct archive --auto off` disables; the setting lives in
`~/.config/cct/config`).

Limitations: archived sessions don't appear in `cct recent` or the transfer
picker, and `cct rename`/`cct transfer` skip them — unarchive first.

## Session retention

Claude Code deletes session transcripts after 30 days by default
(`cleanupPeriodDays`), which silently shrinks searchable history. Reindexing
warns when retention is below 90 days; raise it with `cct cleanup-period 3650`
or silence the warning with `cct cleanup-period --no-warn` (the choice is
stored in `~/.config/cct/config`).

## Trade-offs / non-goals

- No PDF or binary indexing.
- No `--all-tool-results` mode; only the text-like extensions above.
- The catalog is plain TSV (atomic tmp+rename). Schema changes require
  `cct reindex`.
- `path` field is stored on every chunk doc; this is wasteful but simple. A
  future pass can replace it with `file_id → path` lookups in the catalog.
- The daemon is macOS-only; on other platforms `cct daemon install` errors
  out, and `cct daemon update` runs the update without an AC-power gate.

## Tests

```sh
cargo test
```

Integration tests live under `tests/index_tests.rs` and cover:

- full reindex finds normal words,
- trigram finds `APIError` substrings,
- Russian short token `АД` works via word index,
- non-ASCII ≥ 3 chars auto-adds trigram,
- append update only indexes complete jsonl lines,
- partial trailing lines stay invisible until newline appears,
- unchanged update is a no-op,
- deleted files lose hits,
- shrink/rewrite force a full reindex of the file,
- path search finds sessions by their path,
- snippets come from the chunk byte range,
- PDFs and other binaries are skipped,
- an oversized token is found through its indexed prefix, while a different
  blob sharing the same 32-char prefix is rejected by the snippet pass,
- a 31-char identifier and a 25-char Cyrillic word are indexed whole,
- relevance ranking puts a deep old match above a shallow fresh one, while
  `--sort time` still returns newest first,
- two equally strong matches are separated by recency.

`src/blame.rs` unit-tests path recognition: qualified vs bare mentions,
namesake rejection (`research.rsync`), and suffix generation.
