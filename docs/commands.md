# Command reference

`cct <command> --help` lists every flag.

| Command | Alias | What it does | Main flags |
|---------|-------|--------------|------------|
| `cct` / `cct recent` | `r` | Recent sessions in the current project, archived ones included | `-n`, `--headless` |
| `cct search <query>...` | `s` | Full-text search; a bare `cct <query>` works too ([docs](search.md)) | `-g`, `-p`, `-n`, `-c`, `-v`, `-f`, `-s`, `--since`, `--until`, `--sort`, `--mode`, `--headless`, `--json`, `--no-update` |
| `cct show <id> [@loc]` | | Read a session around a match, a `--time`, or its end ([docs](search.md#finding-a-place-inside-a-session)) | `-C`, `--time`, `--full` |
| `cct blame <path>` | `b` | Sessions that edited or discussed a file ([docs](blame.md)) | `-g`, `-p`, `-n`, `-c`, `-v`, `--headless`, `--json`, `--no-update` |
| `cct rename [old] <new>` | `rn` | Move a project directory and migrate its sessions ([docs](projects.md)) | `--no-move-dir`, `--dry-run` |
| `cct transfer <session>... <target>` | `tr` | Move sessions into another project ([docs](projects.md#transfer)) | `--from`, `-y`, `--dry-run` |
| `cct archive` | | Move idle sessions into a zstd archive, still searchable ([docs](archive.md)) | `--older-than`, `--headless-older-than`, `-p`, `--dry-run`, `-y`, `--auto`, `--auto-headless` |
| `cct unarchive <id>...` | | Restore archived sessions | `--keep-mtime` |
| `cct cleanup-period [days]` | `cp` | Show or set Claude Code's transcript retention | `--no-warn`, `--warn` |
| `cct daemon install` / `uninstall` | `d` | launchd job: background updates every 30 min on AC power (macOS) | |
| `cct daemon update` | | The daemon's periodic run; safe to run by hand | |
| `cct update` | `u` | Incremental index update (searches do this themselves) | |
| `cct reindex` | `ri` | Rebuild the index from scratch | `--memory-mb`, `--only` |
| `cct compact` | `c` | Merge index segments down to `--target-segments` (default 1) | |
