//! Git repository access through gitoxide: what protect needs to know about the working copy.

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: not inside a git repository")]
    NotARepo(PathBuf),
    #[error("{path}: git: {reason}")]
    Git { path: PathBuf, reason: String },
    #[error("git has no author identity; set user.name and user.email")]
    NoIdentity,
}

pub type Result<T> = std::result::Result<T, Error>;

/// The state of a working copy, read once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Short branch name; `None` on a detached HEAD.
    pub branch: Option<String>,
    /// HEAD commit, full hex; `None` before the first commit.
    pub head: Option<String>,
    /// The commit of the branch's upstream as last fetched; `None` without an upstream.
    pub upstream: Option<Upstream>,
    /// Changed, staged or untracked (not ignored) paths, relative to the work tree, sorted.
    pub dirty: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// e.g. `origin/main`.
    pub name: String,
    pub commit: String,
}

impl State {
    pub fn is_clean(&self) -> bool {
        self.dirty.is_empty()
    }

    /// HEAD is the upstream commit (as of the last fetch).
    pub fn in_sync(&self) -> bool {
        matches!((&self.head, &self.upstream), (Some(h), Some(u)) if *h == u.commit)
    }
}

/// Reads the state of the repository containing `dir`.
pub fn state(dir: &Path) -> Result<State> {
    let repo = gix::discover(dir).map_err(|_| Error::NotARepo(dir.to_owned()))?;
    let git = |e: &dyn std::fmt::Display| Error::Git {
        path: dir.to_owned(),
        reason: e.to_string(),
    };

    let head_ref = repo.head_name().map_err(|e| git(&e))?;
    let branch = head_ref.as_ref().map(|r| r.shorten().to_string());
    let head = repo.head_id().ok().map(|id| id.to_string());

    let upstream = match &head_ref {
        Some(name) => match repo
            .branch_remote_tracking_ref_name(name.as_ref(), gix::remote::Direction::Fetch)
        {
            Some(Ok(tracking)) => match repo
                .try_find_reference(tracking.as_ref())
                .map_err(|e| git(&e))?
            {
                Some(mut r) => Some(Upstream {
                    name: tracking.shorten().to_string(),
                    commit: r.peel_to_id().map_err(|e| git(&e))?.to_string(),
                }),
                // Configured but never fetched.
                None => None,
            },
            Some(Err(e)) => return Err(git(&e)),
            None => None,
        },
        None => None,
    };

    let mut dirty = Vec::new();
    let status = repo
        .status(gix::progress::Discard)
        .map_err(|e| git(&e))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_iter(None)
        .map_err(|e| git(&e))?;
    for item in status {
        let item = item.map_err(|e| git(&e))?;
        dirty.push(item.location().to_string());
    }
    dirty.sort();
    dirty.dedup();

    Ok(State {
        branch,
        head,
        upstream,
        dirty,
    })
}

/// Writes the files of `commit` (full hex id) of the repository containing `dir` into
/// `dest`: a clean copy of the project as it was, for rollback and promote. Submodules are
/// skipped; symlinks are written as files holding their target.
pub fn export(dir: &Path, commit: &str, dest: &Path) -> Result<()> {
    let repo = gix::discover(dir).map_err(|_| Error::NotARepo(dir.to_owned()))?;
    let git = |e: &dyn std::fmt::Display| Error::Git {
        path: dir.to_owned(),
        reason: e.to_string(),
    };
    let id = gix::ObjectId::from_hex(commit.as_bytes()).map_err(|e| git(&e))?;
    let tree = repo
        .find_commit(id)
        .map_err(|e| git(&e))?
        .tree()
        .map_err(|e| git(&e))?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(|e| git(&e))?;
    for entry in recorder.records {
        if !(entry.mode.is_blob() || entry.mode.is_link()) {
            continue;
        }
        let rel = entry.filepath.to_string();
        if rel.split('/').any(|s| s == ".." || s.is_empty()) {
            return Err(git(&format!("unsafe path in commit: {rel}")));
        }
        let path = dest.join(&rel);
        let data = repo
            .find_object(entry.oid)
            .map_err(|e| git(&e))?
            .detach()
            .data;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| git(&e))?;
        }
        std::fs::write(&path, data).map_err(|e| git(&e))?;
    }
    Ok(())
}

/// Commits `paths` (relative to the work tree, added, changed or deleted) with `message` and
/// returns the new HEAD. Uses the system git, as for any commit opk makes: the user's hooks,
/// signing and identity apply. Returns `None` when the paths have no changes.
pub fn commit(dir: &Path, paths: &[String], message: &str) -> Result<Option<String>> {
    let git = |args: &[&str]| -> Result<std::process::Output> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .map_err(|e| Error::Git {
                path: dir.to_owned(),
                reason: format!("cannot run git: {e}"),
            })
    };
    let failed = |what: &str, out: &std::process::Output| Error::Git {
        path: dir.to_owned(),
        reason: format!(
            "git {what}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    };
    // git refuses a path that is neither on disk nor tracked (a deletion already committed).
    let mut ls = vec!["ls-files", "-z", "--"];
    ls.extend(paths.iter().map(String::as_str));
    let out = git(&ls)?;
    let tracked: std::collections::HashSet<&str> = std::str::from_utf8(&out.stdout)
        .unwrap_or_default()
        .split('\0')
        .collect();
    let paths: Vec<&str> = paths
        .iter()
        .map(String::as_str)
        .filter(|p| dir.join(p).exists() || tracked.contains(p))
        .collect();
    if paths.is_empty() {
        return Ok(None);
    }
    let mut add = vec!["add", "-A", "--"];
    add.extend(paths.iter().copied());
    let out = git(&add)?;
    if !out.status.success() {
        return Err(failed("add", &out));
    }
    // Nothing staged: the paths already match HEAD.
    if git(&["diff", "--cached", "--quiet"])?.status.success() {
        return Ok(None);
    }
    let out = git(&["commit", "-q", "-m", message])?;
    if !out.status.success() {
        return Err(failed("commit", &out));
    }
    let out = git(&["rev-parse", "HEAD"])?;
    Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// Creates a repository in `dir` with HEAD on `main`. Returns false (and leaves it alone) when
/// `dir` already is inside a repository.
pub fn init(dir: &Path) -> Result<bool> {
    if gix::discover(dir).is_ok() {
        return Ok(false);
    }
    let git = |e: &dyn std::fmt::Display| Error::Git {
        path: dir.to_owned(),
        reason: e.to_string(),
    };
    gix::init(dir).map_err(|e| git(&e))?;
    std::fs::write(dir.join(".git").join("HEAD"), "ref: refs/heads/main\n").map_err(|e| git(&e))?;
    Ok(true)
}

/// Commits every file of the work tree that `.gitignore` does not exclude as the first
/// commit on HEAD, and makes the index match it. Fails with [`Error::NoIdentity`] when git
/// has no author configured.
pub fn first_commit(dir: &Path, message: &str) -> Result<String> {
    let git = |e: &dyn std::fmt::Display| Error::Git {
        path: dir.to_owned(),
        reason: e.to_string(),
    };
    let repo = gix::open(dir).map_err(|_| Error::NotARepo(dir.to_owned()))?;
    if repo.author().is_none() || repo.committer().is_none() {
        return Err(Error::NoIdentity);
    }
    let walk = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    let mut files = Vec::new();
    for entry in walk {
        let entry = entry.map_err(|e| git(&e))?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    let empty = gix::ObjectId::empty_tree(repo.object_hash());
    let mut editor = repo.edit_tree(empty).map_err(|e| git(&e))?;
    for path in &files {
        let rel = path.strip_prefix(dir).expect("inside dir");
        let rel = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let data = std::fs::read(path).map_err(|e| git(&e))?;
        let blob = repo.write_blob(&data).map_err(|e| git(&e))?.detach();
        let kind = if is_executable(path) {
            gix::object::tree::EntryKind::BlobExecutable
        } else {
            gix::object::tree::EntryKind::Blob
        };
        editor
            .upsert(rel.as_str(), kind, blob)
            .map_err(|e| git(&e))?;
    }
    let tree = editor.write().map_err(|e| git(&e))?.detach();
    let id = repo
        .commit("HEAD", message, tree, std::iter::empty::<gix::ObjectId>())
        .map_err(|e| git(&e))?
        .detach();
    let mut index = repo.index_from_tree(&tree).map_err(|e| git(&e))?;
    index.write(Default::default()).map_err(|e| git(&e))?;
    Ok(id.to_string())
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git is installed");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn init_and_first_commit() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        write(r, ".gitignore", "*.log\n");
        write(r, "a.txt", "a");
        std::fs::create_dir_all(r.join("sub")).unwrap();
        write(&r.join("sub"), "b.txt", "b");
        write(r, "x.log", "ignored");
        assert!(init(r).unwrap());
        assert!(!init(r).unwrap(), "already a repository");
        git(r, &["config", "user.name", "t"]);
        git(r, &["config", "user.email", "t@t"]);
        let id = first_commit(r, "chore: opk init").unwrap();
        let s = state(r).unwrap();
        assert_eq!(
            (s.branch.as_deref(), s.head.as_deref()),
            (Some("main"), Some(id.as_str()))
        );
        assert!(s.is_clean(), "{:?}", s.dirty);
        let files = git(r, &["ls-files"]);
        assert_eq!(files, ".gitignore\na.txt\nsub/b.txt");
        assert_eq!(git(r, &["status", "--porcelain"]), "");
    }

    #[test]
    fn outside_a_repo() {
        let d = tempfile::tempdir().unwrap();
        assert!(matches!(state(d.path()), Err(Error::NotARepo(_))));
    }

    #[test]
    fn branch_cleanliness_and_upstream() {
        let tmp = tempfile::tempdir().unwrap();
        let (origin, w) = (tmp.path().join("origin.git"), tmp.path().join("work"));
        std::fs::create_dir_all(&w).unwrap();
        git(tmp.path(), &["init", "-q", "--bare", "origin.git"]);
        git(&w, &["init", "-q"]);

        let s = state(&w).unwrap();
        assert_eq!(
            (s.branch.as_deref(), s.head.as_deref()),
            (Some("main"), None)
        );

        write(&w, ".gitignore", "*.log\n");
        write(&w, "a.txt", "a");
        git(&w, &["add", "."]);
        git(&w, &["commit", "-qm", "one"]);
        git(&w, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&w, &["push", "-qu", "origin", "main"]);

        write(&w, "debug.log", "ignored");
        let s = state(&w).unwrap();
        assert!(s.is_clean(), "{:?}", s.dirty);
        assert_eq!(s.upstream.as_ref().unwrap().name, "origin/main");
        assert!(s.in_sync());

        write(&w, "a.txt", "changed");
        write(&w, "new.txt", "untracked");
        let s = state(&w).unwrap();
        assert_eq!(s.dirty, ["a.txt", "new.txt"]);

        git(&w, &["commit", "-qam", "two"]);
        let s = state(&w).unwrap();
        assert_eq!(s.dirty, ["new.txt"]);
        assert!(!s.in_sync(), "a local commit is ahead of origin/main");

        let head = state(&w).unwrap().head.unwrap();
        git(&w, &["checkout", "-q", "--detach"]);
        let s = state(&w).unwrap();
        assert_eq!((s.branch, s.upstream), (None, None));

        let out = tmp.path().join("export");
        export(&w, &head, &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("a.txt")).unwrap(),
            "changed"
        );
        assert!(!out.join("new.txt").exists() && !out.join("debug.log").exists());
        assert!(export(&w, "0000000000000000000000000000000000000000", &out).is_err());

        git(&w, &["checkout", "-q", "main"]);
        git(&w, &["config", "user.name", "t"]);
        git(&w, &["config", "user.email", "t@t"]);
        write(&w, "c.txt", "c");
        std::fs::remove_file(w.join("a.txt")).unwrap();
        let paths = ["c.txt".to_owned(), "a.txt".to_owned()];
        let head = commit(&w, &paths, "chore: test").unwrap().unwrap();
        let s = state(&w).unwrap();
        assert_eq!(s.head.as_deref(), Some(head.as_str()));
        assert_eq!(s.dirty, ["new.txt"], "only the given paths are committed");
        assert_eq!(
            commit(&w, &paths, "again").unwrap(),
            None,
            "no changes, no commit"
        );
    }
}
