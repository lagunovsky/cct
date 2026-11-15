# How the index works

## What gets indexed

The corpus is Claude Code's own layout:

```
~/.claude/projects/<encoded-project>/<session>.jsonl
~/.claude/projects/<encoded-project>/<session>/subagents/*.jsonl
~/.claude/projects/<encoded-project>/<session>/tool-results/*.{txt,json,jsonl,md,log,csv,tsv,html,htm,xml}
~/.claude/archive/…/*.zst                      (archived sessions, same layout)
```

PDFs and other binary tool results are not indexed.

From JSONL records only string values are indexed, not keys. Left out:

- **Boilerplate attachments** Claude Code writes into every session — system
  prompt snapshot, skill and tool listings, CLAUDE.md and auto-memory, output
  style, environment, model, date. They are identical from session to session
  (about a fifth of a real corpus), so a word from your instructions would
  otherwise match every session. Attachments that record what happened stay:
  prompts queued while the agent worked, subagent `structured_output`, edited
  files, diagnostics, todos, hook errors, plans. The list is
  `BOILERPLATE_ATTACHMENTS` in `src/text.rs`.
- **Copies**: base64 (thinking signatures, images), per-record uuids and
  timestamps, `api-request*` and `file-history-snapshot` records, queued
  prompts that were later delivered, and `toolUseResult` strings the record's
  `tool_result` already holds (Read output appears there with line numbers;
  both are recognised as the same text).

Snippets are cut from exactly the same fields (`for_each_indexed_string`), so
a session is never found through text it cannot show.

## Layout

```
~/.claude/search-index/
  word/         SimpleTokenizer + LowerCaser, tokens cut to 32 chars; freqs + positions
  trigram/      NgramTokenizer(3,3) + LowerCaser; doc ids only
  path/         trigrams of file paths
  catalog.tsv   per file: file_id, kind, size, mtime_ms, indexed_offset, next_chunk_id,
                tail_hash, prompts, tools, display_bytes, title, path
  manifest.txt  source dir, enabled indexes, writer config used to build it
```

A Tantivy document is one **chunk** of ~512 KB of complete JSONL lines, not a
whole file, and stores its byte range — snippets are produced by reading just
that range back. Each document also carries `file_id` (stable across updates,
so a file is deleted or replaced with one `delete_term`), an exact `project_id`
for project-scoped searches, and a `headless` marker for sessions started with
`claude -p`.

**Token truncation.** In real logs nearly every token over 32 characters is a
base64 blob, a hash or an id: 30% of the vocabulary, 68% of its bytes, most seen
once. The word index keeps only the first 32 characters (counted as characters,
so Cyrillic isn't cut in half). A query for the full string is cut the same
way and matches the same term; the snippet pass then checks the whole string
against the file, so two blobs sharing a prefix can't pass as each other.
Dropping such tokens and relying on trigrams doesn't work: a 512 KB chunk holds
nearly every trigram, so the AND barely narrows anything.

**Positions.** Word postings keep term frequencies and positions. With
positions a multi-word string is a phrase query; without them it is an AND of
words anywhere in a 512 KB chunk, and a phrase of common words matches most of
the corpus. Positions roughly triple the word index (on a 19.7 GB corpus the
whole index is about 2 GB).

**Verification.** Index hits are candidates. They are read back from the files
in rounds that double in size — word hits first, then trigram, then path, each
by score — until `-n` sessions show a snippet for every query string, or 4000
chunks per string have been read. Several query strings are AND-ed over all of
their hits, not over each string's best few.

## Updates

| Change | Action |
|--------|--------|
| new file | add with a new `file_id` |
| `.jsonl` grew | read only `[indexed_offset, last complete newline)`; a partial trailing line waits for the next update |
| shrink, rewrite, non-JSONL change | delete by `file_id`, add again |
| file removed | delete by `file_id`, drop from the catalog |
| unchanged | nothing is read or written |

Before the append fast path is taken, a hash of the indexed prefix's tail is
compared with the catalog, so a file rewritten in place and then grown is
re-indexed instead of appended to.
Appends go to a small **delta** index first; the daemon (and `cct archive`)
flushes the delta into the main index. All index mutations take an exclusive
lock file, so concurrent searches and the daemon don't collide.

After a search, update, daemon run or archive pass the index is tidied. The
delta is merged once a sub-index reaches 8 segments. The main index is merged
once it reaches 16 segments or more than 20% of its documents are deleted
(every flush replaces a grown session and only marks the old copy deleted): the
small segments and any segment more than 30% dead fold into one, the 4 largest
healthy segments are left alone. Each pass touches what accumulated since the
last one, not the whole index.

| Command | Merge policy | Memory |
|---------|--------------|--------|
| `reindex` | `log` (Tantivy default) | physical RAM / 6, clamped to 768–8192 MB |
| `update` | none during the update, then the tidy above | 768 MB total, split per index |
| `compact` | explicit merge to `--target-segments` (default 1) | 768 MB |

`--memory-mb`, `--worker-threads`, `--merge-threads` and `--only word,trigram,path`
override the defaults.

Schema and catalog changes are not backward compatible: rebuild with
`cct reindex`.

## Trade-offs

- No PDF or binary indexing, and only the text-like tool-result extensions
  above.
- The catalog is plain TSV, replaced atomically (tmp + rename).
- The `path` field is stored on every chunk; a `file_id → path` lookup through
  the catalog would be leaner.
- The daemon is macOS-only; elsewhere `cct daemon update` runs the update
  without the AC-power check.
