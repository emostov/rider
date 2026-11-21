use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::board::TaskId;
use crate::git;
use crate::queue_wait::{self, WaitError};

pub const PRS_DIR: &str = "prs";
pub const STORED_SUFFIX: &str = ".pr";
const FILES_MAX: usize = 3000;
const PATH_BYTES_MAX: usize = 1024;
const CALLS_IN_FLIGHT_MAX: usize = 16;
const FETCH_SECONDS_MAX: u64 = 600;

#[derive(Debug)]
pub enum FilesError {
    Call(WaitError),
    Field(&'static str),
    UnsafePath(String),
    Truncated,
    Exists(PathBuf),
    Io { path: PathBuf, problem: String },
}

impl fmt::Display for FilesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilesError::Call(error) => write!(f, "{error}"),
            FilesError::Field(field) => write!(f, "gh reply: {field} is missing or malformed"),
            FilesError::UnsafePath(path) => {
                write!(
                    f,
                    "the pull request has a path rider will not store: {path:?}"
                )
            }
            FilesError::Truncated => write!(
                f,
                "the pull request lists {FILES_MAX} files or more, so GitHub may have cut the \
                 list short"
            ),
            FilesError::Exists(path) => write!(f, "{} already exists", path.display()),
            FilesError::Io { path, problem } => write!(f, "{}: {problem}", path.display()),
        }
    }
}

impl std::error::Error for FilesError {}

fn safe_path(path: &str) -> bool {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"._-+@".contains(&b);
    let component =
        |part: &str| !part.is_empty() && part != "." && part != ".." && part.bytes().all(allowed);
    !path.is_empty()
        && path.len() <= PATH_BYTES_MAX
        && !path.starts_with('-')
        && path.split('/').all(component)
}

fn changed_paths(reply: &[u8]) -> Result<Vec<String>, FilesError> {
    let pages: Value = serde_json::from_slice(reply).map_err(|_| FilesError::Field("files"))?;
    let pages = pages.as_array().ok_or(FilesError::Field("files"))?;
    let mut paths = Vec::new();
    for entry in pages
        .iter()
        .flat_map(|page| page.as_array().into_iter().flatten())
    {
        if paths.len() >= FILES_MAX {
            return Err(FilesError::Truncated);
        }
        let name = entry["filename"]
            .as_str()
            .ok_or(FilesError::Field("filename"))?;
        let status = entry["status"]
            .as_str()
            .ok_or(FilesError::Field("status"))?;
        if !safe_path(name) {
            return Err(FilesError::UnsafePath(name.to_string()));
        }
        if status != "removed" {
            paths.push(name.to_string());
        }
    }
    assert!(paths.len() <= FILES_MAX);
    assert!(paths.iter().all(|path| safe_path(path)));
    Ok(paths)
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), FilesError> {
    assert!(path.is_absolute());
    let problem = |error: std::io::Error| FilesError::Io {
        path: path.to_path_buf(),
        problem: error.to_string(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(problem)?;
    }
    fs::write(path, bytes).map_err(problem)
}

// Every file is stored under its path plus a suffix, so no stored name is `HEAD`, `config`
// or another file git reads as a repository's own.
pub fn fetch(
    home: &Path,
    item: TaskId,
    repo: &str,
    number: u64,
    base: &str,
    head: &str,
) -> Result<PathBuf, FilesError> {
    assert!(git::is_commit_id(base) && git::is_commit_id(head));
    assert!(!repo.starts_with('-') && repo.contains('/'));
    let deadline = Instant::now() + Duration::from_secs(FETCH_SECONDS_MAX);
    let dir = home.join(PRS_DIR).join(format!("{}-{head}", item.0));
    if fs::symlink_metadata(&dir).is_ok() {
        return Err(FilesError::Exists(dir));
    }
    let gh = |args: &[&str]| queue_wait::run("gh", args, deadline).map_err(FilesError::Call);
    let list = format!("repos/{repo}/pulls/{number}/files?per_page=100");
    let paths = changed_paths(&gh(&["api", "--paginate", "--slurp", &list])?)?;
    let compare = format!("repos/{repo}/compare/{base}...{head}");
    let diff = gh(&["api", "-H", "Accept: application/vnd.github.diff", &compare])?;
    let mut contents = Vec::with_capacity(paths.len());
    for batch in paths.chunks(CALLS_IN_FLIGHT_MAX) {
        let fetched = thread::scope(|scope| {
            let calls: Vec<_> = (batch.iter())
                .map(|path| {
                    let url = format!("repos/{repo}/contents/{path}?ref={head}");
                    scope.spawn(move || {
                        gh(&["api", "-H", "Accept: application/vnd.github.raw", &url])
                    })
                })
                .collect();
            let joined = calls.into_iter().map(|call| {
                let panicked = Err(FilesError::Call(WaitError::PollPanicked));
                call.join().unwrap_or(panicked)
            });
            joined.collect::<Result<Vec<_>, FilesError>>()
        });
        contents.extend(fetched?);
    }
    assert_eq!(contents.len(), paths.len());
    write(&dir.join("patch.diff"), &diff)?;
    for (path, bytes) in paths.iter().zip(&contents) {
        write(
            &dir.join("files").join(format!("{path}{STORED_SUFFIX}")),
            bytes,
        )?;
    }
    Ok(dir)
}
