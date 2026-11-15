# Development

```sh
cargo build --release
cargo test --release
```

Tests never write to `~/.claude`: they build indexes and project trees in temp
directories.

- `tests/index_tests.rs` — indexing and search end to end: word, trigram and
  path queries, phrases and AND across strings past the candidate limit,
  append updates and partial trailing lines, deletes and rewrites, ranking and
  `--sort time`, oversized tokens and prefix collisions, boilerplate and
  duplicate exclusion, headless filtering, archived sessions, searching inside
  one session with time windows.
- `tests/rename_transfer_tests.rs` — `rename` and `transfer` against an
  injected projects directory: cwd rewriting at path boundaries, subdirectories,
  titles, dry runs, finding sessions in any project.
- `tests/archive_tests.rs` — archive / unarchive round trips, subdirectories,
  separate ages for headless sessions.
- Unit tests next to the code: text extraction and line classification
  (`src/text.rs`), path recognition for blame (`src/blame.rs`), project
  directory encoding (`src/path_codec.rs`), `cct show` locations and time
  parsing (`src/show.rs`).
