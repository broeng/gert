//! The one place that runs `git`, so no call can forget the safety flags.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use crate::Error;

/// Options in front of every command: read the repository as it is, never
/// follow replace refs, never take locks or touch the index for an
/// optional refresh, and never start a file system monitor daemon.
const GLOBAL_ARGS: &[&str] = &[
    "--no-replace-objects",
    "--no-optional-locks",
    "--no-pager",
    "-c",
    "core.fsmonitor=false",
];

/// Environment variables that would point git at another repository.
const REMOVED_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
];

/// A git invocation helper bound to one repository directory.
///
/// Blocking: every call spawns a process and waits for it.
#[derive(Clone, Debug)]
pub(crate) struct Git {
    dir: PathBuf,
}

impl Git {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs `git -C <dir> <GLOBAL_ARGS> <args>` and returns the output
    /// whatever the exit status. `stdin` is written to the process, or it
    /// gets a closed stdin. Callers pass `--no-ext-diff --no-textconv`
    /// themselves on diff commands.
    pub(crate) fn run_raw(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Output, Error> {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir).args(GLOBAL_ARGS).args(args);
        for var in REMOVED_ENV {
            cmd.env_remove(var);
        }
        cmd.env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_PAGER", "cat")
            .env("GIT_OPTIONAL_LOCKS", "0")
            // Paths are paths, never patterns or `:(magic)`.
            .env("GIT_LITERAL_PATHSPECS", "1")
            // Stable messages, since we recognise a few of them.
            .env("LC_ALL", "C");
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        log::trace!("git {}", args.join(" "));
        let mut child = cmd.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::GitNotFound,
            _ => Error::Io(e),
        })?;
        let input = child.stdin.take();
        std::thread::scope(|s| {
            // Written from a thread so a big answer cannot deadlock us
            // against a full pipe. A broken pipe just means git was done.
            if let (Some(mut pipe), Some(data)) = (input, stdin) {
                s.spawn(move || {
                    let _ = pipe.write_all(data);
                });
            }
            child.wait_with_output().map_err(Error::Io)
        })
    }

    /// Like [`run_raw`](Self::run_raw), but a non-zero exit is an error.
    pub(crate) fn run(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Vec<u8>, Error> {
        let out = self.run_raw(args, stdin)?;
        if out.status.success() {
            Ok(out.stdout)
        } else {
            Err(failure(args, &out))
        }
    }
}

/// The error for a command that exited unsuccessfully.
pub(crate) fn failure(args: &[&str], out: &Output) -> Error {
    Error::Git {
        command: args.first().copied().unwrap_or_default().to_owned(),
        status: out
            .status
            .code()
            .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
    }
}

/// One answer of `git cat-file --batch`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Object {
    Missing,
    Found {
        oid: String,
        kind: String,
        data: Vec<u8>,
    },
}

impl Git {
    /// Reads objects by name (`<commit>:<path>`, `<oid>`) with a single
    /// `cat-file --batch`. Names must not contain a newline. The result has
    /// one entry per name.
    pub(crate) fn read_objects(&self, names: &[String]) -> Result<Vec<Object>, Error> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut input = names.join("\n").into_bytes();
        input.push(b'\n');
        let out = self.run(&["cat-file", "--batch"], Some(&input))?;
        let bad = |detail: String| Error::Parse {
            command: "cat-file --batch",
            detail,
        };
        let mut pos = 0;
        let mut objects = Vec::with_capacity(names.len());
        for _ in names {
            let nl = out[pos..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or_else(|| bad("truncated header".into()))?;
            let header = String::from_utf8_lossy(&out[pos..pos + nl]).into_owned();
            pos += nl + 1;
            let parts: Vec<&str> = header.split(' ').collect();
            match parts.as_slice() {
                [.., "missing"] | [.., "ambiguous"] => objects.push(Object::Missing),
                [oid, kind, size] => {
                    let size: usize = size
                        .parse()
                        .map_err(|_| bad(format!("bad size in {header:?}")))?;
                    let data = out
                        .get(pos..pos + size)
                        .ok_or_else(|| bad("truncated content".into()))?
                        .to_vec();
                    pos += size + 1; // the content is followed by a newline
                    objects.push(Object::Found {
                        oid: (*oid).to_owned(),
                        kind: (*kind).to_owned(),
                        data,
                    });
                }
                _ => return Err(bad(format!("unrecognised header {header:?}"))),
            }
        }
        Ok(objects)
    }
}
