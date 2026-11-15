# Moving projects

Claude Code keeps a project's sessions in `~/.claude/projects/<encoded-path>/`,
and every record carries the project's absolute path as `cwd`. Rename or move
the project directory and `/resume` no longer finds them. `cct rename` and
`cct transfer` move sessions to the new path and rewrite the old one inside
them.

## rename

Moves the project directory and migrates all of its sessions in one step:

```sh
cct rename ../new-name                    # old = cwd
cct rename /old/abs /new/abs              # explicit
cct rename --no-move-dir . ../new         # dir already moved by hand; only fix sessions
cct rename --dry-run . ../new             # preview
```

It moves the directory on disk, renames `~/.claude/projects/<encoded-old>` to
`<encoded-new>`, and rewrites the old absolute path everywhere in the session
files — `cwd` included, and only at path boundaries, so `/app` doesn't touch
`/app2`. The next search re-indexes the moved files.

## transfer

Moves individual sessions into another project:

```sh
cct transfer abc123 ../other              # by full id or unique prefix, from any project
cct transfer abc123 def456 ../other       # several; the target always comes last
cct transfer abc123,def456 ../other       # comma-separated works too
cct transfer my-session ../other          # by /rename title
cct transfer --from /old/proj dev ../other  # a title several projects share
cct transfer ../other                     # pick sessions from cwd interactively
cct transfer --dry-run abc123 ../other    # preview
cct transfer -y abc123 ../other           # skip the confirmation
```

An id names one session wherever it lives, so no source project is needed:
the session is found under `~/.claude/projects`, and the path rewritten to the
target comes from its own `cwd` records. A title is looked up in the current
project first, then in every project through the index; when several projects
have a session by that name, the error lists them and `--from` picks one.

The picker takes Space to toggle, `a` / `A` to select all / none, `/` to
filter, Enter to confirm.

Archived sessions are skipped by both commands — `cct unarchive` them first.
