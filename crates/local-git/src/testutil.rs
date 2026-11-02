//! Temporary repositories built with the git CLI. Panicking on failure is
//! the point here.
#![allow(clippy::expect_used, clippy::new_without_default)]

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

pub struct TestRepo {
    pub dir: TempDir,
}

impl TestRepo {
    pub fn new() -> Self {
        let r = Self {
            dir: tempfile::tempdir().expect("temp dir"),
        };
        r.git(&["init", "-q", "-b", "main"]);
        r
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs git with a hermetic environment and returns trimmed stdout.
    pub fn git(&self, args: &[&str]) -> String {
        git_in(self.path(), args)
    }

    pub fn write(&self, path: &str, content: &[u8]) {
        let full = self.path().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(full, content).expect("write");
    }

    pub fn remove(&self, path: &str) {
        std::fs::remove_file(self.path().join(path)).expect("remove");
    }

    /// Commits everything in the work tree and returns the new sha.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["rev-parse", "HEAD"])
    }
}

pub fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test Author")
        .env("GIT_AUTHOR_EMAIL", "author@example.com")
        .env("GIT_COMMITTER_NAME", "Test Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.com")
        .env("GIT_AUTHOR_DATE", "2013-02-01T09:59:32+0100")
        .env("GIT_COMMITTER_DATE", "2013-02-01T09:59:32+0100")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}
