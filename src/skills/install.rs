use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{SKILLS, SkillsError, VERSION, skill_files};

const MANIFEST_FILE: &str = ".rider-install.json";
const RECORD_FILE: &str = "skill-installs";
const MANIFEST_BYTES_MAX: u64 = 64 << 10;
const SKILL_FILE_BYTES_MAX: u64 = 1 << 20;
const SKILL_FILES_MAX: usize = 64;
const INSTALLS_MAX: usize = 32;
const RECORD_BYTES_MAX: u64 = 64 << 10;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Install,
    Update,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed,
    UpToDate,
    Replaced { from: String },
    Edited(Vec<String>),
    Foreign,
    Newer(String),
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: String,
    digest: String,
    files: BTreeMap<String, String>,
}

fn digest(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    });
    let text = format!("{hash:016x}");
    assert_eq!(text.len(), 16);
    text
}

fn embedded(skill: &str) -> Manifest {
    let files: BTreeMap<String, String> = skill_files(skill)
        .map(|(path, text)| (path.to_string(), digest(text.as_bytes())))
        .collect();
    assert!(!files.is_empty() && files.len() <= SKILL_FILES_MAX);
    assert!(version_key(VERSION).is_some());
    let listing: String = files
        .iter()
        .map(|(path, sum)| format!("{path}\0{sum}\n"))
        .collect();
    Manifest {
        version: VERSION.to_string(),
        digest: digest(listing.as_bytes()),
        files,
    }
}

fn io_error(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> SkillsError {
    let path = path.to_path_buf();
    move |error| SkillsError::Io {
        action,
        path,
        message: error.to_string(),
    }
}

fn read_bounded(path: &Path, most: u64) -> Result<Option<Vec<u8>>, SkillsError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("open", path)(error)),
    };
    let mut bytes = Vec::new();
    let limit = most
        .checked_add(1)
        .ok_or(SkillsError::TooLarge(path.to_path_buf()))?;
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(io_error("read", path))?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > most) {
        return Err(SkillsError::TooLarge(path.to_path_buf()));
    }
    Ok(Some(bytes))
}

fn on_disk(root: &Path) -> Result<BTreeMap<String, Option<String>>, SkillsError> {
    let mut files = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    for _ in 0..SKILL_FILES_MAX {
        let Some(dir) = pending.pop() else {
            return Ok(files);
        };
        for entry in fs::read_dir(&dir).map_err(io_error("read", &dir))? {
            let path = entry.map_err(io_error("read", &dir))?.path();
            let kind = fs::symlink_metadata(&path).map_err(io_error("inspect", &path))?;
            let kind = kind.file_type();
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let relative = relative.to_string_lossy().into_owned();
            if relative == MANIFEST_FILE {
                continue;
            }
            let sum = match kind.is_file() {
                true => read_bounded(&path, SKILL_FILE_BYTES_MAX)?.map(|bytes| digest(&bytes)),
                false => None,
            };
            files.insert(relative, sum);
            if files.len() > SKILL_FILES_MAX {
                return Err(SkillsError::TooManyFiles(root.to_path_buf()));
            }
        }
    }
    Err(SkillsError::TooManyFiles(root.to_path_buf()))
}

fn edits(manifest: &Manifest, found: &BTreeMap<String, Option<String>>) -> Vec<String> {
    let mut edited: Vec<String> = manifest
        .files
        .iter()
        .filter(|(path, sum)| found.get(*path) != Some(&Some((*sum).clone())))
        .map(|(path, _)| path.clone())
        .collect();
    let added = found
        .keys()
        .filter(|path| !manifest.files.contains_key(*path));
    edited.extend(added.cloned());
    edited.sort_unstable();
    assert!(edited.len() <= manifest.files.len().saturating_add(found.len()));
    edited
}

fn version_key(version: &str) -> Option<[u64; 3]> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let key = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(key)
}

fn installed(skill_dir: &Path) -> Result<Option<Manifest>, SkillsError> {
    let path = skill_dir.join(MANIFEST_FILE);
    let Some(bytes) = read_bounded(&path, MANIFEST_BYTES_MAX)? else {
        return Ok(None);
    };
    let bad = |why: String| SkillsError::Manifest {
        path: path.clone(),
        why,
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    if version_key(&manifest.version).is_none() {
        return Err(bad(format!("{:?} is not a version", manifest.version)));
    }
    Ok(Some(manifest))
}

fn install_skill(dir: &Path, skill: &str, mode: Mode) -> Result<Outcome, SkillsError> {
    let skill_dir = dir.join(skill);
    let ours = embedded(skill);
    match fs::symlink_metadata(&skill_dir) {
        Ok(meta) if !meta.is_dir() => return Ok(Outcome::Foreign),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if mode == Mode::Update {
                return Ok(Outcome::Absent);
            }
            write_skill(dir, skill, &ours, false)?;
            return Ok(Outcome::Installed);
        }
        Err(error) => return Err(io_error("inspect", &skill_dir)(error)),
    }
    let Some(theirs) = installed(&skill_dir)? else {
        return Ok(Outcome::Foreign);
    };
    let edited = edits(&theirs, &on_disk(&skill_dir)?);
    if !edited.is_empty() {
        return Ok(Outcome::Edited(edited));
    }
    if theirs.digest == ours.digest {
        return Ok(Outcome::UpToDate);
    }
    if version_key(&theirs.version) > version_key(VERSION) {
        return Ok(Outcome::Newer(theirs.version));
    }
    write_skill(dir, skill, &ours, true)?;
    Ok(Outcome::Replaced {
        from: theirs.version,
    })
}

fn write_skill(
    dir: &Path,
    skill: &str,
    manifest: &Manifest,
    replace: bool,
) -> Result<(), SkillsError> {
    let staging = dir.join(format!(".{skill}.rider-new"));
    let aside = dir.join(format!(".{skill}.rider-old"));
    let target = dir.join(skill);
    for leftover in [&staging, &aside] {
        match fs::remove_dir_all(leftover) {
            Err(error) if error.kind() != ErrorKind::NotFound => {
                return Err(io_error("remove", leftover)(error));
            }
            _ => {}
        }
    }
    for (path, text) in skill_files(skill) {
        assert!(
            !Path::new(path)
                .components()
                .any(|c| c == Component::ParentDir)
        );
        let file = staging.join(path);
        let parent = file.parent().unwrap_or(&staging);
        fs::create_dir_all(parent).map_err(io_error("create", parent))?;
        fs::write(&file, text).map_err(io_error("write", &file))?;
    }
    let json = serde_json::to_string_pretty(manifest).map_err(|error| SkillsError::Manifest {
        path: staging.join(MANIFEST_FILE),
        why: error.to_string(),
    })?;
    let path = staging.join(MANIFEST_FILE);
    fs::write(&path, json).map_err(io_error("write", &path))?;
    if replace {
        fs::rename(&target, &aside).map_err(io_error("move aside", &target))?;
    }
    if let Err(error) = fs::rename(&staging, &target) {
        if replace {
            fs::rename(&aside, &target).map_err(io_error("move back", &aside))?;
        }
        return Err(io_error("publish", &target)(error));
    }
    if replace {
        fs::remove_dir_all(&aside).map_err(io_error("remove", &aside))?;
    }
    assert!(target.join(MANIFEST_FILE).is_file());
    Ok(())
}

pub type Report = Vec<(PathBuf, Result<Outcome, SkillsError>)>;

pub fn install_into(dir: &Path, mode: Mode) -> Report {
    assert!(dir.is_absolute());
    let created = match mode {
        Mode::Install => fs::create_dir_all(dir).map_err(io_error("create", dir)),
        Mode::Update => Ok(()),
    };
    let report: Report = SKILLS
        .iter()
        .map(|skill| {
            let outcome = match &created {
                Ok(()) => install_skill(dir, skill, mode),
                Err(error) => Err(error.clone()),
            };
            (dir.join(skill), outcome)
        })
        .collect();
    assert_eq!(report.len(), SKILLS.len());
    report
}

pub fn resolve(into: &Path) -> Result<PathBuf, SkillsError> {
    if into.components().any(|c| c == Component::ParentDir) {
        return Err(SkillsError::BadInto(into.to_path_buf()));
    }
    let here = env::current_dir().map_err(io_error("find", Path::new(".")))?;
    let dir: PathBuf = here.join(into).components().collect();
    assert!(dir.is_absolute());
    Ok(dir)
}

pub fn recorded(home: &Path) -> Result<Vec<PathBuf>, SkillsError> {
    let path = home.join(RECORD_FILE);
    let Some(bytes) = read_bounded(&path, RECORD_BYTES_MAX)? else {
        return Ok(Vec::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| SkillsError::BadRecord(path.clone()))?;
    let mut dirs = Vec::new();
    for line in text.lines() {
        let dir = PathBuf::from(line);
        if !dir.is_absolute() {
            return Err(SkillsError::BadRecord(path));
        }
        if dirs.len() == INSTALLS_MAX {
            return Err(SkillsError::TooManyInstalls(path));
        }
        dirs.push(dir);
    }
    assert!(dirs.len() <= INSTALLS_MAX);
    Ok(dirs)
}

pub fn record(home: &Path, dir: &Path) -> Result<(), SkillsError> {
    assert!(dir.is_absolute() && dir.to_str().is_some());
    let mut dirs = recorded(home)?;
    if dirs.iter().any(|known| known == dir) {
        return Ok(());
    }
    let path = home.join(RECORD_FILE);
    if dirs.len() == INSTALLS_MAX {
        return Err(SkillsError::TooManyInstalls(path));
    }
    dirs.push(dir.to_path_buf());
    let text: String = dirs
        .iter()
        .map(|dir| format!("{}\n", dir.display()))
        .collect();
    let fresh = home.join(format!("{RECORD_FILE}.new"));
    fs::create_dir_all(home).map_err(io_error("create", home))?;
    fs::write(&fresh, text).map_err(io_error("write", &fresh))?;
    fs::rename(&fresh, &path).map_err(io_error("write", &path))
}

pub fn update_recorded(home: &Path, except: Option<&Path>) -> Result<Report, SkillsError> {
    let dirs = recorded(home)?;
    let report: Report = dirs
        .iter()
        .filter(|dir| Some(dir.as_path()) != except)
        .flat_map(|dir| install_into(dir, Mode::Update))
        .collect();
    assert!(report.len() <= dirs.len().saturating_mul(SKILLS.len()));
    Ok(report)
}
