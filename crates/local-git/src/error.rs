use std::path::PathBuf;

/// A real failure while talking to git. These are meant to be shown to the
/// user (the settings page), so the texts are complete sentences.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the git executable was not found; install git or add it to PATH")]
    GitNotFound,
    #[error("{} is not a git repository", .0.display())]
    NotARepository(PathBuf),
    #[error("git {command} failed ({status}): {stderr}")]
    Git {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("could not run git: {0}")]
    Io(#[source] std::io::Error),
    #[error("unexpected output from git {command}: {detail}")]
    Parse {
        command: &'static str,
        detail: String,
    },
}

/// Why something has to come from Gerrit instead of the checkout.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotLocal {
    #[error("commit {0} is not in the checkout")]
    MissingCommit(String),
    #[error("Gerrit compares a merge commit with its auto-merge")]
    MergeCommit,
    #[error("the patch sets have different parents, so Gerrit marks rebased edits")]
    ParentsDiffer,
    #[error("Gerrit synthesizes {0}")]
    SpecialFile(String),
    #[error("the whitespace mode is not known")]
    UnknownWhitespace,
    #[error("the path {0:?} cannot be read from a checkout")]
    UnsupportedPath(String),
    #[error("{0} is not a regular file in the checkout")]
    NotAFile(String),
    /// The file is there, but its content is not (a partial clone).
    #[error("the content of {0} is not in the checkout")]
    MissingContent(String),
}
