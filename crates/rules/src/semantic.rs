//! What the world means: a record for each thing a request made (a build, a land
//! change, a game package and its feature files, the pieces games put down) and for
//! places people named, so an agent coming to the world later finds and understands
//! what's there by asking for it ("the wolves", "anything about bread near me").
//!
//! Most of it is already in the log: a build's name and the request that made it, a land
//! change's name and area, a package's title and files. [`Action::Describe`] adds what
//! the agent says it means (a line about it and some tags), or names an area that has
//! nothing built. The latest description of a thing wins; an empty one takes it away.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    log::{Action, Builds, WorldLog},
    logic::Logic,
    terrain::VOXEL_SIZE,
};

/// What something means, as an agent described it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Meaning {
    /// What it's about: `build:<id>`, `land:<change>`, `package:<name>`,
    /// `feature:<package>/<file>`, or `place:<name>` for an area someone named.
    pub of: String,
    pub name: String,
    /// One line on what it is and what it's for. Empty takes the description away.
    pub about: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Where a place is: its middle and size across the ground, meters `[x, z, w, d]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<[f32; 4]>,
}

/// One thing in the world, as an agent searching it sees it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Record {
    /// As in [`Meaning::of`], or `piece:<id>` for a game's piece.
    pub of: String,
    pub kind: &'static str,
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub about: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The request that made it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    /// Who made it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub by: String,
    /// Where it is, meters (its middle, on the ground).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<[f32; 3]>,
    /// How far it reaches across the ground from `at`, meters.
    #[serde(skip_serializing_if = "is_zero")]
    pub reach: f32,
    /// The change that made it (or last changed it).
    pub change: u64,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// Everything the world means, as of its log: builds still standing, land changes,
/// installed packages and their files, labeled pieces, and named places.
pub fn records(log: &WorldLog, builds: &Builds, logic: &Logic) -> Vec<Record> {
    // The latest description of each thing.
    let mut meanings: BTreeMap<String, Meaning> = BTreeMap::new();
    let mut made: BTreeMap<String, (u64, String, String)> = BTreeMap::new();
    let mut land: Vec<Record> = Vec::new();
    for change in &log.changes {
        let prompt = change.by.prompt.clone().unwrap_or_default();
        let by = change.by.player_name.clone();
        match &change.action {
            Action::Describe { meaning } => {
                if meaning.about.trim().is_empty() && meaning.name.trim().is_empty() {
                    meanings.remove(&meaning.of);
                } else {
                    meanings.insert(meaning.of.clone(), meaning.clone());
                }
            }
            Action::PlaceBuild(b) | Action::UpdateBuild(b) => {
                made.insert(format!("build:{}", b.build), (change.id, prompt, by));
            }
            Action::Terrain { name, area, spec, .. } => {
                let size = [area.size[0] as f32 * VOXEL_SIZE, area.size[1] as f32 * VOXEL_SIZE];
                let middle = [area.min[0] as f32 * VOXEL_SIZE + size[0] / 2.0, area.min[1] as f32 * VOXEL_SIZE + size[1] / 2.0];
                land.push(Record {
                    of: format!("land:{}", change.id),
                    kind: "land",
                    name: name.clone(),
                    about: spec.about.clone().unwrap_or_default(),
                    tags: Vec::new(),
                    prompt,
                    by,
                    at: Some([middle[0], 0.0, middle[1]]),
                    reach: size[0].hypot(size[1]) / 2.0,
                    change: change.id,
                });
            }
            Action::InstallPackage { package, .. } => {
                made.insert(format!("package:{}", package.name), (change.id, prompt, by));
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (id, b) in builds.iter() {
        let of = format!("build:{id}");
        let (change, prompt, by) = made.get(&of).cloned().unwrap_or_default();
        let side = |i: usize| b.spec.size.get(i).copied().unwrap_or(1) as f32 * crate::build::VOXEL_SIZE;
        out.push(Record {
            of,
            kind: "build",
            name: b.name.clone(),
            about: b.spec.about.clone().unwrap_or_default(),
            tags: Vec::new(),
            prompt: if prompt.is_empty() { b.prompt.clone() } else { prompt },
            by,
            at: Some(b.position),
            reach: side(0).hypot(side(2)) / 2.0,
            change,
        });
    }
    out.extend(land);
    for (name, install) in &logic.installed {
        let Some(package) = logic.package(install) else { continue };
        let of = format!("package:{name}");
        let (change, prompt, by) = made.get(&of).cloned().unwrap_or_default();
        out.push(Record {
            of,
            kind: "package",
            name: package.manifest.title.clone(),
            about: String::new(),
            tags: Vec::new(),
            prompt: prompt.clone(),
            by: by.clone(),
            at: None,
            reach: 0.0,
            change,
        });
        for (file, source) in &package.scripts {
            // Where a feature's pieces are.
            let feature = format!("feature:{name}/{file}");
            let pieces: Vec<[f32; 3]> =
                logic.state.entities().filter(|(_, e)| e.package == *name && e.components.get("feature").and_then(|f| f.as_str()) == Some(file)).map(|(_, e)| e.position).collect();
            let at = (!pieces.is_empty()).then(|| {
                let n = pieces.len() as f32;
                std::array::from_fn(|i| pieces.iter().map(|p| p[i]).sum::<f32>() / n)
            });
            let reach = at.map_or(0.0, |a: [f32; 3]| pieces.iter().map(|p| (p[0] - a[0]).hypot(p[2] - a[2])).fold(0.0, f32::max));
            out.push(Record { of: feature, kind: "feature", name: file.clone(), about: leading_comment(source), tags: Vec::new(), prompt: String::new(), by: by.clone(), at, reach, change });
        }
    }
    for (id, e) in logic.state.entities() {
        let Some(label) = e.components.get("label").and_then(|l| l.as_str()) else { continue };
        let model = e.components.get("look").and_then(|l| l["model"].as_str()).unwrap_or_default();
        out.push(Record {
            of: format!("piece:{}.{}", id.change, id.n),
            kind: "piece",
            name: label.to_string(),
            about: String::new(),
            tags: if model.is_empty() { Vec::new() } else { vec![model.to_string()] },
            prompt: String::new(),
            by: e.package.clone(),
            at: Some(e.position),
            reach: 0.0,
            change: id.change,
        });
    }
    // What agents said things mean; places are only what they said.
    for record in &mut out {
        if let Some(m) = meanings.remove(&record.of) {
            if !m.name.trim().is_empty() {
                record.name = m.name;
            }
            record.about = m.about;
            record.tags.extend(m.tags);
        }
    }
    for (of, m) in meanings {
        if !of.starts_with("place:") {
            continue;
        }
        let [x, z, w, d] = m.area.unwrap_or_default();
        out.push(Record { of, kind: "place", name: m.name, about: m.about, tags: m.tags, prompt: String::new(), by: String::new(), at: Some([x, 0.0, z]), reach: w.hypot(d) / 2.0, change: 0 });
    }
    out
}

/// What a script file says it is in its opening comment lines (`-- Wolves roam the
/// field east of camp`).
fn leading_comment(source: &str) -> String {
    let lines: Vec<&str> = source.lines().map(str::trim).take_while(|l| l.starts_with("--")).map(|l| l.trim_start_matches('-').trim()).collect();
    lines.join(" ").chars().take(400).collect()
}

/// A record found, how well it matched, and why.
#[derive(Clone, Debug, Serialize)]
pub struct Found {
    #[serde(flatten)]
    pub record: Record,
    pub score: f32,
    /// Meters from where the search was centered, if it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    pub matched: Vec<String>,
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(|w| {
            let w = w.to_lowercase();
            // Plurals find singulars: "wolves" finds "wolf", "rocks" "rock".
            if let Some(stem) = w.strip_suffix("ves") {
                format!("{stem}f")
            } else if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w
            }
        })
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

const STOP: [&str; 24] = [
    "the", "an", "and", "or", "of", "to", "in", "on", "at", "for", "with", "is", "it", "me", "my", "near", "here", "where", "what", "any",
    "anything", "about", "that", "this",
];

/// Finds records about `text`, best first: words matched in the name count most, then
/// tags, what it's about, and the request that made it; rare words count more than
/// common ones. With `near` (x, z), closer things rank higher, and `radius` (meters)
/// leaves out what's further. Empty `text` lists what's near, nearest first.
pub fn search(records: &[Record], text: &str, near: Option<[f32; 2]>, radius: Option<f32>, limit: usize) -> Vec<Found> {
    let query = words(text);
    let fields = |r: &Record| -> [(Vec<String>, f32); 4] {
        [(words(&r.name), 3.0), (words(&r.tags.join(" ")), 2.0), (words(&r.about), 1.5), (words(&r.prompt), 1.0)]
    };
    // How many records each word turns up in: rare words say more.
    let indexed: Vec<[(Vec<String>, f32); 4]> = records.iter().map(fields).collect();
    let rarity = |w: &str| {
        let n = indexed.iter().filter(|f| f.iter().any(|(ws, _)| ws.iter().any(|x| x == w))).count() as f32;
        (1.0 + records.len() as f32 / (1.0 + n)).ln()
    };
    let mut found: Vec<Found> = records
        .iter()
        .zip(&indexed)
        .filter_map(|(r, f)| {
            let distance = near.zip(r.at).map(|(n, a)| ((a[0] - n[0]).hypot(a[2] - n[1]) - r.reach).max(0.0));
            if radius.zip(distance).is_some_and(|(radius, d)| d > radius) || (radius.is_some() && near.is_some() && distance.is_none()) {
                return None;
            }
            let mut score = 0.0;
            let mut matched = Vec::new();
            for w in &query {
                let best = f.iter().filter(|(ws, _)| ws.iter().any(|x| x == w || (x.len() > 3 && w.len() > 3 && (x.starts_with(w.as_str()) || w.starts_with(x.as_str()))))).map(|(_, weight)| *weight).fold(0.0, f32::max);
                if best > 0.0 {
                    score += best * rarity(w);
                    matched.push(w.clone());
                }
            }
            if !query.is_empty() && score == 0.0 {
                return None;
            }
            // Nearer is better: a little over half the score at 30 m, a quarter far away.
            let nearness = distance.map_or(1.0, |d| 30.0 / (30.0 + d));
            score = if query.is_empty() { nearness } else { score * (0.25 + 0.75 * nearness) };
            Some(Found { record: r.clone(), score, distance, matched })
        })
        .collect();
    found.sort_by(|a, b| b.score.total_cmp(&a.score));
    found.truncate(limit);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(of: &str, name: &str, about: &str, at: [f32; 3]) -> Record {
        Record { of: of.into(), kind: "build", name: name.into(), about: about.into(), tags: Vec::new(), prompt: String::new(), by: String::new(), at: Some(at), reach: 1.0, change: 1 }
    }

    #[test]
    fn search_finds_by_meaning_and_place() {
        let records = vec![
            record("build:1", "Bakery", "where the baker sells bread", [0.0, 0.0, 0.0]),
            record("build:2", "Wolf den", "a rocky den the wolves roam from", [40.0, 0.0, 0.0]),
            record("build:3", "Bread oven", "an old oven", [100.0, 0.0, 0.0]),
            record("build:4", "Fence", "keeps the sheep in", [5.0, 0.0, 5.0]),
        ];
        let names = |found: Vec<Found>| found.into_iter().map(|f| f.record.name).collect::<Vec<_>>();
        assert_eq!(names(search(&records, "where are the wolves", None, None, 5)), vec!["Wolf den"]);
        // Both are about bread; the nearer one first, and a radius leaves the far one out.
        assert_eq!(names(search(&records, "bread", Some([0.0, 0.0]), None, 5)), vec!["Bakery", "Bread oven"]);
        assert_eq!(names(search(&records, "anything about bread near me", Some([0.0, 0.0]), Some(20.0), 5)), vec!["Bakery"]);
        // Nothing asked: what's near, nearest first.
        assert_eq!(names(search(&records, "", Some([0.0, 0.0]), Some(10.0), 5)), vec!["Bakery", "Fence"]);
    }
}
