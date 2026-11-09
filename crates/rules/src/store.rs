//! Keeping a [`WorldLog`] on disk, append-only: `world.jsonl` holds the log's header,
//! then one line per package and per change, in order. Saving after a change writes
//! just that change, so saving costs the same in a world with a million changes as in
//! a new one. A crash mid-write leaves at most a broken last line, which loading skips.
//!
//! Rare changes to the header (a new admin) or to history itself (signing old changes)
//! rewrite the whole file, atomically. A `world.json` from before is read once and
//! moved aside as `world.json.bak`.

use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::{
    log::{Change, Snapshot, WorldLog, WorldState, state_at},
    logic::Package,
    settings::WorldRules,
};

/// The log without its changes and packages: the first line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Head {
    seed: u32,
    rules_version: u32,
    #[serde(default)]
    admins: Vec<String>,
    #[serde(default)]
    rules: WorldRules,
    #[serde(default)]
    flat: bool,
}

impl Head {
    fn of(log: &WorldLog) -> Self {
        Self { seed: log.seed, rules_version: log.rules_version, admins: log.admins.clone(), rules: log.rules.clone(), flat: log.flat }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Line {
    Head(Head),
    Package(Arc<Package>),
    Change(Change),
}

/// A log's file, and how much of the log is in it.
#[derive(Debug)]
pub struct LogFile {
    path: PathBuf,
    head: Option<Head>,
    /// Changes written, and the last one's id.
    changes: usize,
    last: Option<u64>,
    packages: HashSet<String>,
}

impl LogFile {
    /// The log kept in `dir`, read if it's there (or migrated from `world.json`).
    pub fn open(dir: &Path) -> io::Result<(Self, Option<WorldLog>)> {
        let mut file = Self { path: dir.join("world.jsonl"), head: None, changes: 0, last: None, packages: HashSet::new() };
        if file.path.exists() {
            let log = read_lines(&file.path)?;
            file.mark(&log);
            return Ok((file, Some(log)));
        }
        let old = dir.join("world.json");
        let Ok(text) = fs::read_to_string(&old) else { return Ok((file, None)) };
        let log: WorldLog = serde_json::from_str(&text).map_err(io::Error::other)?;
        file.rewrite(&log)?;
        fs::rename(&old, dir.join("world.json.bak"))?;
        Ok((file, Some(log)))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Notes that the file holds all of `log`.
    fn mark(&mut self, log: &WorldLog) {
        self.head = Some(Head::of(log));
        self.changes = log.changes.len();
        self.last = log.changes.last().map(|c| c.id);
        self.packages = log.packages.keys().cloned().collect();
    }

    /// Writes what `log` has that the file doesn't: new changes and packages, appended.
    /// A new header, or a log that isn't the file's plus more, rewrites it.
    pub fn sync(&mut self, log: &WorldLog) -> io::Result<()> {
        let extends = self.changes <= log.changes.len()
            && self.last == self.changes.checked_sub(1).map(|i| log.changes[i].id)
            && self.head.as_ref() == Some(&Head::of(log));
        if !extends {
            return self.rewrite(log);
        }
        let mut lines = Vec::new();
        for (hash, package) in &log.packages {
            if !self.packages.contains(hash) {
                lines.push(serde_json::to_string(&Line::Package(package.clone())).map_err(io::Error::other)?);
            }
        }
        for change in &log.changes[self.changes..] {
            lines.push(serde_json::to_string(&Line::Change(change.clone())).map_err(io::Error::other)?);
        }
        if lines.is_empty() {
            return Ok(());
        }
        let mut out = OpenOptions::new().append(true).open(&self.path)?;
        out.write_all((lines.join("\n") + "\n").as_bytes())?;
        out.flush()?;
        self.mark(log);
        Ok(())
    }

    /// Writes the whole log again, replacing the file at once.
    pub fn rewrite(&mut self, log: &WorldLog) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = serde_json::to_string(&Line::Head(Head::of(log))).map_err(io::Error::other)? + "\n";
        for package in log.packages.values() {
            text += &(serde_json::to_string(&Line::Package(package.clone())).map_err(io::Error::other)? + "\n");
        }
        for change in &log.changes {
            text += &(serde_json::to_string(&Line::Change(change.clone())).map_err(io::Error::other)? + "\n");
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)?;
        self.mark(log);
        Ok(())
    }
}

/// The game kept in `dir` (`game.json`), if there is one.
pub fn read_game(dir: &Path) -> io::Result<Option<crate::game::GameLog>> {
    match fs::read_to_string(dir.join("game.json")) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Saves `game` in `dir`, replacing the file at once. Games change rarely (a world made,
/// a save), so the whole file is written each time.
pub fn write_game(dir: &Path, game: &crate::game::GameLog) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let tmp = dir.join("game.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(game).map_err(io::Error::other)?)?;
    fs::rename(&tmp, dir.join("game.json"))
}

/// Keeps the world in `dir` as of a save (`snapshot.upto`, the save's change) in
/// `states/<change>.json`, so loading the save swaps it in instead of replaying.
pub fn write_state(dir: &Path, snapshot: &Snapshot) -> io::Result<()> {
    let dir = dir.join("states");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", snapshot.upto));
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(snapshot).map_err(io::Error::other)?)?;
    fs::rename(&tmp, path)
}

/// The world in `dir`, whose log is `log`, as of its change `save`: as it was kept then
/// ([`write_state`]), or else replayed up to it (from `snapshot`, if that's older).
pub fn state_of_save(dir: &Path, log: &WorldLog, save: u64, snapshot: Option<&Snapshot>) -> WorldState {
    let kept = fs::read_to_string(dir.join("states").join(format!("{save}.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Snapshot>(&text).ok())
        .filter(|s| s.upto == save && s.fits(log));
    match kept {
        Some(kept) => kept.state,
        None => state_at(log, save, snapshot),
    }
}

/// A world at a glance, for lists: read without loading its changes.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub seed: u32,
    pub flat: bool,
    pub changes: usize,
    pub modified: Option<std::time::SystemTime>,
}

/// The world kept in `dir`, at a glance, if there is one.
pub fn summary(dir: &Path) -> Option<Summary> {
    let path = dir.join("world.jsonl");
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    if let Ok(file) = fs::File::open(&path) {
        let mut lines = BufReader::new(file).lines().map_while(Result::ok);
        let Line::Head(head) = serde_json::from_str(&lines.next()?).ok()? else { return None };
        let changes = lines.filter(|l| l.starts_with("{\"change\"")).count();
        return Some(Summary { seed: head.seed, flat: head.flat, changes, modified: modified(&path) });
    }
    let old = dir.join("world.json");
    let log: WorldLog = serde_json::from_str(&fs::read_to_string(&old).ok()?).ok()?;
    Some(Summary { seed: log.seed, flat: log.flat, changes: log.changes.len(), modified: modified(&old) })
}

fn read_lines(path: &Path) -> io::Result<WorldLog> {
    let lines: Vec<String> = BufReader::new(fs::File::open(path)?).lines().collect::<io::Result<_>>()?;
    let mut log: Option<WorldLog> = None;
    let count = lines.len();
    for (i, text) in lines.iter().enumerate() {
        if text.trim().is_empty() {
            continue;
        }
        let line = match serde_json::from_str::<Line>(text) {
            Ok(line) => line,
            // The last line may be half written, if a save was cut off.
            Err(_) if i + 1 == count => break,
            Err(e) => return Err(io::Error::other(format!("{}:{}: {e}", path.display(), i + 1))),
        };
        match (line, &mut log) {
            (Line::Head(h), None) => {
                let mut fresh = WorldLog::new(h.seed);
                fresh.rules_version = h.rules_version;
                fresh.admins = h.admins;
                fresh.rules = h.rules;
                fresh.flat = h.flat;
                log = Some(fresh);
            }
            (Line::Package(p), Some(log)) => {
                log.packages.insert(p.hash(), p);
            }
            (Line::Change(c), Some(log)) => log.changes.push(c),
            _ => return Err(io::Error::other(format!("{}:{}: out of place", path.display(), i + 1))),
        }
    }
    log.ok_or_else(|| io::Error::other(format!("{} has no header", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Action, Provenance};

    #[test]
    fn appends_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("genverse-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut log = WorldLog::new(9);
        log.admins.push("a".into());
        // An old single-file log moves over.
        fs::write(dir.join("world.json"), serde_json::to_string(&log).unwrap()).unwrap();
        let (mut file, read) = LogFile::open(&dir).unwrap();
        assert_eq!(read.as_ref(), Some(&log));
        assert!(dir.join("world.json.bak").exists());
        let claim = |n: i32| Action::ClaimParcels { parcels: vec![[n, 0]] };
        log.push(Provenance::new("a", "a", None, None, 1), claim(1));
        file.sync(&log).unwrap();
        log.push(Provenance::new("a", "a", None, None, 2), claim(2));
        file.sync(&log).unwrap();
        let size = fs::metadata(file.path()).unwrap().len();
        assert_eq!(LogFile::open(&dir).unwrap().1.unwrap(), log);
        // A new admin rewrites; a cut-off last line is skipped.
        log.admins.push("b".into());
        file.sync(&log).unwrap();
        assert!(fs::metadata(file.path()).unwrap().len() > size);
        let mut out = OpenOptions::new().append(true).open(file.path()).unwrap();
        out.write_all(b"{\"change\":{\"id\":3,").unwrap();
        assert_eq!(LogFile::open(&dir).unwrap().1.unwrap(), log);
        let _ = fs::remove_dir_all(&dir);
    }
}
