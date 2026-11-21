use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::process;

use fs4::FileExt;
use serde::{Deserialize, Serialize};

use super::{SKILLS, SkillsError, VERSION, skill_files};

const MANIFEST_FILE: &str = ".rider-install.json";
const RECORD_FILE: &str = "skill-installs";
const HOME_LOCK_FILE: &str = "skill-installs.lock";
const DIR_LOCK_FILE: &str = ".rider-install.lock";
const ASIDE_SUFFIX: &str = ".rider-old";
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

fn regular(path: &Path) -> Result<bool, SkillsError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(SkillsError::NotRegular(path.to_path_buf())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect", path)(error)),
    }
}

fn read_bounded(path: &Path, most: u64) -> Result<Option<Vec<u8>>, SkillsError> {
    if !regular(path)? {
        return Ok(None);
    }
    let file = File::open(path).map_err(io_error("open", path))?;
    let meta = file.metadata().map_err(io_error("inspect", path))?;
    if !meta.is_file() {
        return Err(SkillsError::NotRegular(path.to_path_buf()));
    }
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

fn lock(path: &Path) -> Result<File, SkillsError> {
    regular(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(io_error("open", path))?;
    FileExt::lock(&file).map_err(io_error("lock", path))?;
    assert!(file.metadata().is_ok_and(|meta| meta.is_file()));
    Ok(file)
}

fn recover(dir: &Path, skill: &str) -> Result<(), SkillsError> {
    let target = dir.join(skill);
    let aside = dir.join(format!(".{skill}{ASIDE_SUFFIX}"));
    let absent = |path: &Path| match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(io_error("inspect", path)(error)),
    };
    let aside_is_ours = match fs::symlink_metadata(&aside) {
        Ok(meta) => meta.is_dir() && regular(&aside.join(MANIFEST_FILE))?,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(io_error("inspect", &aside)(error)),
    };
    if aside_is_ours && absent(&target)? {
        fs::rename(&aside, &target).map_err(io_error("move back", &aside))?;
    }
    assert!(!aside_is_ours || target.exists() || aside.exists());
    Ok(())
}

fn install_skill(dir: &Path, skill: &str, mode: Mode) -> Result<Outcome, SkillsError> {
    recover(dir, skill)?;
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
    let staging = dir.join(format!(".{skill}.rider-new-{}", process::id()));
    let aside = dir.join(format!(".{skill}{ASIDE_SUFFIX}"));
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallDir(PathBuf);

impl InstallDir {
    pub fn parse(path: &Path) -> Result<InstallDir, SkillsError> {
        let bad = || SkillsError::BadInto(path.to_path_buf());
        let text = path.to_str().ok_or_else(bad)?;
        let safe = path.is_absolute()
            && !text.contains(['\n', '\r'])
            && !path.components().any(|c| c == Component::ParentDir);
        if !safe {
            return Err(bad());
        }
        let dir: PathBuf = path.components().collect();
        assert!(dir.is_absolute() && dir.to_str().is_some());
        Ok(InstallDir(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

pub fn resolve(into: &Path) -> Result<InstallDir, SkillsError> {
    let here = env::current_dir().map_err(io_error("find", Path::new(".")))?;
    InstallDir::parse(&here.join(into))
}

fn install_into(dir: &InstallDir, mode: Mode) -> Report {
    let dir = dir.path();
    let locked = match mode {
        Mode::Install => fs::create_dir_all(dir).map_err(io_error("create", dir)),
        Mode::Update => Ok(()),
    }
    .and_then(|()| lock(&dir.join(DIR_LOCK_FILE)));
    let report: Report = SKILLS
        .iter()
        .map(|skill| {
            let outcome = match &locked {
                Ok(_) => install_skill(dir, skill, mode),
                Err(error) => Err(error.clone()),
            };
            (dir.join(skill), outcome)
        })
        .collect();
    assert_eq!(report.len(), SKILLS.len());
    report
}

fn recorded(home: &Path) -> Result<(Vec<InstallDir>, bool), SkillsError> {
    let path = home.join(RECORD_FILE);
    let Some(bytes) = read_bounded(&path, RECORD_BYTES_MAX)? else {
        return Ok((Vec::new(), false));
    };
    let text = String::from_utf8(bytes).map_err(|_| SkillsError::BadRecord(path.clone()))?;
    let mut dirs = Vec::new();
    let mut dropped = false;
    for line in text.lines() {
        let dir =
            InstallDir::parse(Path::new(line)).map_err(|_| SkillsError::BadRecord(path.clone()))?;
        if dirs.len() == INSTALLS_MAX {
            return Err(SkillsError::TooManyInstalls(path));
        }
        match fs::symlink_metadata(dir.path()) {
            Err(error) if error.kind() == ErrorKind::NotFound => dropped = true,
            _ => dirs.push(dir),
        }
    }
    assert!(dirs.len() <= INSTALLS_MAX);
    Ok((dirs, dropped))
}

fn write_record(home: &Path, dirs: &[InstallDir]) -> Result<(), SkillsError> {
    assert!(dirs.len() <= INSTALLS_MAX);
    let text: String = dirs
        .iter()
        .map(|dir| format!("{}\n", dir.path().display()))
        .collect();
    let path = home.join(RECORD_FILE);
    let fresh = home.join(format!("{RECORD_FILE}.new"));
    regular(&fresh)?;
    fs::write(&fresh, text).map_err(io_error("write", &fresh))?;
    fs::rename(&fresh, &path).map_err(io_error("write", &path))
}

fn lock_home(home: &Path) -> Result<File, SkillsError> {
    fs::create_dir_all(home).map_err(io_error("create", home))?;
    lock(&home.join(HOME_LOCK_FILE))
}

pub fn install(home: &Path, dir: &InstallDir) -> Result<(Report, Report), SkillsError> {
    let _home = lock_home(home)?;
    let (mut dirs, dropped) = recorded(home)?;
    let known = dirs.contains(dir);
    if !known && dirs.len() == INSTALLS_MAX {
        return Err(SkillsError::TooManyInstalls(home.join(RECORD_FILE)));
    }
    let report = install_into(dir, Mode::Install);
    let landed = report.iter().any(|(_, outcome)| {
        matches!(
            outcome,
            Ok(Outcome::Installed
                | Outcome::UpToDate
                | Outcome::Replaced { .. }
                | Outcome::Newer(_))
        )
    });
    if landed && !known {
        dirs.push(dir.clone());
    }
    if dropped || (landed && !known) {
        write_record(home, &dirs)?;
    }
    let others: Report = dirs
        .iter()
        .filter(|other| *other != dir)
        .flat_map(|other| install_into(other, Mode::Update))
        .collect();
    assert!(others.len() <= dirs.len().saturating_mul(SKILLS.len()));
    Ok((report, others))
}

pub fn update_recorded(home: &Path) -> Result<Report, SkillsError> {
    let _home = lock_home(home)?;
    let (dirs, dropped) = recorded(home)?;
    if dropped {
        write_record(home, &dirs)?;
    }
    let report: Report = dirs
        .iter()
        .flat_map(|dir| install_into(dir, Mode::Update))
        .collect();
    assert!(report.len() <= dirs.len().saturating_mul(SKILLS.len()));
    Ok(report)
}
