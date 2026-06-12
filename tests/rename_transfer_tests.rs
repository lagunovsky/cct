//! Workflow tests for `cct rename` and `cct transfer`.
//!
//! Both functions normally read `~/.claude/projects/`. The `*_in` variants in
//! `cct::rename` / `cct::transfer` accept an explicit `projects` directory so
//! these tests can drive everything inside a tempdir.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cct::path_codec::encode_project_dir;
use cct::rename::{rename_project_in, RenameArgs};
use cct::transfer::transfer_sessions_in;

struct WorkspaceFixture {
    _tmp: tempfile::TempDir,
    projects: PathBuf,
    work: PathBuf,
}

impl WorkspaceFixture {
    fn new() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let projects = tmp.path().join("projects");
        fs::create_dir_all(&projects)?;
        let work = tmp.path().join("work");
        fs::create_dir_all(&work)?;
        Ok(Self {
            _tmp: tmp,
            projects,
            work,
        })
    }

    /// Materialize a real directory at `<work>/<name>` plus the encoded session dir
    /// `<projects>/<encoded>` containing one .jsonl file with `cwd` set to the
    /// project's absolute path. Returns the absolute project path.
    fn make_project(&self, name: &str, sessions: &[(&str, &str)]) -> Result<PathBuf> {
        let project_abs = self.work.join(name);
        fs::create_dir_all(&project_abs)?;
        let encoded = encode_project_dir(&project_abs.to_string_lossy());
        let sess_dir = self.projects.join(encoded);
        fs::create_dir_all(&sess_dir)?;
        for (sess_id, body) in sessions {
            // Each session gets one jsonl line that mentions the abs project path
            // so we can verify cwd-rewrites end-to-end.
            let line = format!(
                r#"{{"type":"user","cwd":"{}","timestamp":"2026-01-15T12:00:00Z","message":{{"content":"{}"}}}}"#,
                project_abs.display(),
                body.replace('"', "\\\"")
            );
            let file = sess_dir.join(format!("{sess_id}.jsonl"));
            fs::write(&file, format!("{line}\n"))?;
        }
        Ok(project_abs)
    }

    fn session_dir_for(&self, project_abs: &Path) -> PathBuf {
        self.projects
            .join(encode_project_dir(&project_abs.to_string_lossy()))
    }
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path).expect("read")
}

#[test]
fn rename_dry_run_changes_nothing() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let old = fx.make_project("old-name", &[("session-A", "first prompt")])?;
    let new = fx.work.join("new-name");
    let old_sess = fx.session_dir_for(&old);

    rename_project_in(
        RenameArgs {
            old_arg: old.to_string_lossy().into_owned(),
            new_arg: new.to_string_lossy().into_owned(),
            move_dir: false,
            dry_run: true,
        },
        &fx.projects,
    )?;

    assert!(
        old.exists(),
        "dry-run must not touch the project dir: {}",
        old.display()
    );
    assert!(
        old_sess.exists(),
        "dry-run must not move the session dir: {}",
        old_sess.display()
    );
    let body = read_to_string(&old_sess.join("session-A.jsonl"));
    assert!(
        body.contains(&old.to_string_lossy().into_owned()),
        "dry-run must not rewrite cwd"
    );
    Ok(())
}

#[test]
fn rename_moves_sessions_and_rewrites_cwd() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let old = fx.make_project("project-a", &[("session-1", "hi from a")])?;
    let new = fx.work.join("project-b");

    rename_project_in(
        RenameArgs {
            old_arg: old.to_string_lossy().into_owned(),
            new_arg: new.to_string_lossy().into_owned(),
            move_dir: false,
            dry_run: false,
        },
        &fx.projects,
    )?;

    let old_sess = fx.session_dir_for(&old);
    let new_sess = fx.session_dir_for(&new);
    assert!(
        !old_sess.exists(),
        "old session dir should be gone: {}",
        old_sess.display()
    );
    assert!(
        new_sess.exists(),
        "new session dir must exist: {}",
        new_sess.display()
    );

    let body = read_to_string(&new_sess.join("session-1.jsonl"));
    assert!(
        body.contains(&new.to_string_lossy().into_owned()),
        "new cwd missing in jsonl: {body}"
    );
    assert!(
        !body.contains(&old.to_string_lossy().into_owned()),
        "old cwd must be rewritten away: {body}"
    );
    // --move-dir was false so the on-disk project stays put.
    assert!(old.exists());
    assert!(!new.exists());
    Ok(())
}

#[test]
fn rename_cleans_dot_and_parent_components_without_resolving_symlinks() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let old = fx.make_project("project-dot", &[("session-dot", "hi")])?;
    let new = fx.work.join("renamed-dot");

    rename_project_in(
        RenameArgs {
            old_arg: old.join(".").to_string_lossy().into_owned(),
            new_arg: old
                .join("..")
                .join("renamed-dot")
                .to_string_lossy()
                .into_owned(),
            move_dir: false,
            dry_run: false,
        },
        &fx.projects,
    )?;

    let new_sess = fx.session_dir_for(&new);
    assert!(new_sess.join("session-dot.jsonl").exists());
    let body = read_to_string(&new_sess.join("session-dot.jsonl"));
    assert!(body.contains(&new.to_string_lossy().into_owned()));
    assert!(!body.contains("/../"));
    Ok(())
}

#[test]
fn rename_with_move_dir_renames_project_dir_too() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let old = fx.make_project("alpha", &[("session-x", "hello alpha")])?;
    let new = fx.work.join("beta");

    rename_project_in(
        RenameArgs {
            old_arg: old.to_string_lossy().into_owned(),
            new_arg: new.to_string_lossy().into_owned(),
            move_dir: true,
            dry_run: false,
        },
        &fx.projects,
    )?;

    assert!(!old.exists(), "old dir should be moved away");
    assert!(new.exists(), "new dir must exist");
    assert!(fx.session_dir_for(&new).exists());
    Ok(())
}

#[test]
fn rename_does_not_touch_sibling_paths_sharing_a_prefix() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let old = fx.make_project("proj", &[("session-p", "main project")])?;
    let sibling = fx.work.join("proj2");
    let sibling_str = sibling.to_string_lossy().into_owned();
    let new = fx.work.join("renamed");

    // The session log also references the sibling project `proj2` and a path
    // INSIDE the renamed project; only the latter may be rewritten.
    let old_sess = fx.session_dir_for(&old);
    let file = old_sess.join("session-p.jsonl");
    let mut content = read_to_string(&file);
    content.push_str(&format!(
        r#"{{"type":"user","cwd":"{sibling_str}","message":{{"content":"see {inner}"}}}}"#,
        inner = old.join("src/main.rs").display()
    ));
    content.push('\n');
    fs::write(&file, content)?;

    rename_project_in(
        RenameArgs {
            old_arg: old.to_string_lossy().into_owned(),
            new_arg: new.to_string_lossy().into_owned(),
            move_dir: false,
            dry_run: false,
        },
        &fx.projects,
    )?;

    let body = read_to_string(&fx.session_dir_for(&new).join("session-p.jsonl"));
    assert!(
        body.contains(&sibling_str),
        "sibling path proj2 must stay untouched: {body}"
    );
    assert!(
        body.contains(&new.join("src/main.rs").to_string_lossy().into_owned()),
        "path inside the project must be rewritten: {body}"
    );
    Ok(())
}

#[test]
fn rename_refuses_when_target_session_dir_exists() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let a = fx.make_project("a", &[("s1", "x")])?;
    let b = fx.make_project("b", &[("s2", "y")])?;

    let res = rename_project_in(
        RenameArgs {
            old_arg: a.to_string_lossy().into_owned(),
            new_arg: b.to_string_lossy().into_owned(),
            move_dir: false,
            dry_run: false,
        },
        &fx.projects,
    );
    assert!(res.is_err(), "rename into existing target must fail");
    // Both still exist
    assert!(fx.session_dir_for(&a).exists());
    assert!(fx.session_dir_for(&b).exists());
    Ok(())
}

#[test]
fn transfer_relocates_jsonl_and_rewrites_cwd() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let src = fx.make_project(
        "src",
        &[("aaaa1111", "moved prompt"), ("bbbb2222", "stays")],
    )?;
    let dst = fx.work.join("dst");
    fs::create_dir_all(&dst)?;

    transfer_sessions_in(&["aaaa1111".to_string()], &src, &dst, false, &fx.projects)?;

    let src_sess = fx.session_dir_for(&src);
    let dst_sess = fx.session_dir_for(&dst);
    assert!(!src_sess.join("aaaa1111.jsonl").exists());
    assert!(dst_sess.join("aaaa1111.jsonl").exists());
    assert!(
        src_sess.join("bbbb2222.jsonl").exists(),
        "untouched session must stay"
    );

    let moved = read_to_string(&dst_sess.join("aaaa1111.jsonl"));
    assert!(
        moved.contains(&dst.to_string_lossy().into_owned()),
        "cwd must be rewritten to dst: {moved}"
    );
    assert!(
        !moved.contains(&src.to_string_lossy().into_owned()),
        "old cwd must be gone: {moved}"
    );
    Ok(())
}

#[test]
fn transfer_resolves_session_by_title() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let src = fx.make_project("srctitle", &[("dddd4444", "named session")])?;
    let dst = fx.work.join("dsttitle");
    fs::create_dir_all(&dst)?;

    // Append a `/rename` custom-title record to the session log.
    let src_sess = fx.session_dir_for(&src);
    let file = src_sess.join("dddd4444.jsonl");
    let mut content = read_to_string(&file);
    content.push_str(r#"{"type":"custom-title","customTitle":"my-cool-name","sessionId":"dddd4444"}"#);
    content.push('\n');
    fs::write(&file, content)?;

    // Transfer by the human name rather than the id.
    transfer_sessions_in(&["my-cool-name".to_string()], &src, &dst, false, &fx.projects)?;

    let dst_sess = fx.session_dir_for(&dst);
    assert!(
        dst_sess.join("dddd4444.jsonl").exists(),
        "session should move when resolved by title"
    );
    assert!(!src_sess.join("dddd4444.jsonl").exists());
    Ok(())
}

#[test]
fn transfer_carries_subdirs_and_rewrites_their_files() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let src = fx.make_project("src2", &[("ccc33333", "with subdir")])?;
    let dst = fx.work.join("dst2");
    fs::create_dir_all(&dst)?;

    let src_sess = fx.session_dir_for(&src);
    let subdir = src_sess.join("ccc33333").join("tool-results");
    fs::create_dir_all(&subdir)?;
    let tool_payload = format!(
        "log line referring to old cwd {}\nsecond line\n",
        src.display()
    );
    fs::write(subdir.join("trace.log"), &tool_payload)?;

    transfer_sessions_in(&["ccc33333".to_string()], &src, &dst, false, &fx.projects)?;

    let dst_sess = fx.session_dir_for(&dst);
    let new_log = dst_sess
        .join("ccc33333")
        .join("tool-results")
        .join("trace.log");
    assert!(
        new_log.exists(),
        "subdir must follow: {}",
        new_log.display()
    );
    let body = read_to_string(&new_log);
    assert!(
        body.contains(&dst.to_string_lossy().into_owned()),
        "subdir file must have rewritten cwd: {body}"
    );
    assert!(
        !body.contains(&src.to_string_lossy().into_owned()),
        "old cwd in subdir file must be gone: {body}"
    );
    Ok(())
}

#[test]
fn transfer_dry_run_changes_nothing() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let src = fx.make_project("dryrun-src", &[("ddd44444", "dry run target")])?;
    let dst = fx.work.join("dryrun-dst");
    fs::create_dir_all(&dst)?;

    transfer_sessions_in(&["ddd44444".to_string()], &src, &dst, true, &fx.projects)?;

    let src_sess = fx.session_dir_for(&src);
    let dst_sess = fx.session_dir_for(&dst);
    assert!(
        src_sess.join("ddd44444.jsonl").exists(),
        "src jsonl must remain"
    );
    assert!(
        !dst_sess.exists(),
        "dst session dir must not be created on dry-run"
    );
    Ok(())
}

#[test]
fn transfer_into_same_project_is_an_error() -> Result<()> {
    let fx = WorkspaceFixture::new()?;
    let p = fx.make_project("same", &[("eee55555", "x")])?;

    let res = transfer_sessions_in(
        &["eee55555".to_string()],
        &p,
        &p.join("."),
        false,
        &fx.projects,
    );
    assert!(res.is_err(), "src == dst must fail");
    Ok(())
}
