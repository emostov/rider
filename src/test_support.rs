//! Helpers shared by unit tests.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Writes `text` to `path` as an executable that may be run at once, even while other threads
/// spawn children.
pub fn write_executable(path: &Path, text: &str) {
    // A write descriptor open in this process is inherited by a child another thread forks and
    // stays open there until that child execs; running the file meanwhile fails with ETXTBSY.
    // So a child of its own writes the file, using only builtins so an empty PATH still works.
    let status = Command::new("/bin/sh")
        .args(["-c", r#"printf %s "$2" > "$1""#, "sh"])
        .arg(path)
        .arg(text)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "writing {} failed: {status}",
        path.display()
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), text);
}
