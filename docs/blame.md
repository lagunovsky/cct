# Blame

`cct blame <path>` answers the other direction: not "where was this word
discussed" but "which sessions worked on this file, and what did they say".

```sh
cct blame src/index/search.rs      # sessions in the current project
cct blame -g Cargo.toml            # across all projects
cct blame --json src/cli.rs        # one object per session, plus "mention"
```

Transcripts mention paths as ordinary text, so blame searches for the file's
name and then checks every hit. Sessions come in three tiers:

1. **edited** — the session changed the file: an `Edit` / `Write` / `MultiEdit`
   call on it, or an `edited_text_file` record. Claude Code writes the latter
   when a file the session has seen changes on disk, and it is the only trace
   an edit through Bash, `sed` or a python script leaves. These sessions print
   an `edited <path>` line, and the record shows up as an `[edited]` snippet.
2. **qualified** — the session wrote `src/index/search.rs` or a longer tail of
   the path.
3. **bare** — only `search.rs`. A name inside a longer word (`research.rsync`)
   doesn't count.

Within a tier the usual [relevance order](search.md#result-order) applies.
`--json` reports the tier as `"mention": "edited" | "qualified" | "bare"`.

Edits count only for this very file: an existing path is resolved to its
absolute form, so an edit of a namesake in another directory or project is a
mention at most. `cct blame ./cli.rs` and `cct blame src/cli.rs` behave the
same. A path that no longer exists still works — it is matched as typed, which
is the point when the file was deleted months ago.

Edits of CLAUDE.md and auto-memory files are not counted from
`edited_text_file`: Claude Code broadcasts those into every session open at
the time. The session that made the edit still shows it through its own
`Edit` / `Write` call.

Flags: `-n` (sessions, default 10), `-c`, `-v`, `-g`, `-p`, `--headless`,
`--no-update`, `--json` — as in [search](search.md).
