//! Chat commands for looking after the world, until the history panel exists:
//!
//! - `/history [n]`: the latest changes, with ids, who made them, and how.
//! - `/revert <id>`: undo one of your own changes (anyone's, for admins).
//! - `/rules`: the world's rules.
//! - Admins: `/rollback <player>` undoes everything a player did; `/restore <id>` puts
//!   the world back how it was right after change `id`; `/land open|parcels`;
//!   `/rate <changes per minute> [voxels per 10 minutes]` or `/rate off`.
//!
//! Anything that undoes changes other than your latest rebuilds the world from the log.

use bevy::prelude::*;
use cubelark_rules::{
    log::{Action, replay},
    settings::{LandMode, RateLimits},
};

use super::{creations::Creations, landscape::Landscape};
use crate::world::{VoxelWorld, ledger::Ledger};

pub struct Context<'a, 'w, 's> {
    pub commands: &'a mut Commands<'w, 's>,
    pub ledger: &'a mut Ledger,
    pub world: &'a mut VoxelWorld,
    pub creations: &'a mut Creations,
    pub landscape: &'a mut Landscape,
}

const HELP: &str = "Commands: /history [n] · /revert <id> · /rules · admins: /rollback <player> · \
/restore <id> · /land open|parcels · /rate <changes/min> [voxels/10 min] | off";

/// Runs `/command args` and returns the reply for the chat.
pub fn run(text: &str, cx: &mut Context) -> String {
    let mut words = text.trim_start_matches('/').split_whitespace();
    let name = words.next().unwrap_or_default().to_lowercase();
    let args: Vec<&str> = words.collect();
    let admin = cx.ledger.land.is_admin(&cx.ledger.profile.id);
    let busy = cx.creations.draft.is_some() || cx.landscape.draft.is_some();
    match name.as_str() {
        "history" => history(cx.ledger, args.first().and_then(|n| n.parse().ok()).unwrap_or(12)),
        "rules" => rules(cx.ledger),
        "revert" | "rollback" | "restore" if busy => "Keep or discard your draft first.".into(),
        "revert" => {
            let Some(id) = args.first().and_then(|n| n.parse::<u64>().ok()) else {
                return "Usage: /revert <change id> (see /history)".into();
            };
            if !cx.ledger.log.changes.iter().any(|c| c.id == id) {
                return format!("There's no change {id}.");
            }
            if !cx.ledger.log.may_revert(&cx.ledger.profile.id, id) {
                return "You can only undo your own changes.".into();
            }
            if cx.ledger.log.undone().contains(&id) {
                return format!("Change {id} is already undone.");
            }
            cx.ledger.revert(id);
            rebuild(cx);
            format!("Undid change {id}.")
        }
        "rollback" | "restore" | "land" | "rate" if !admin => "Only admins can do that.".into(),
        "rollback" => {
            let Some(who) = args.first().map(|w| w.to_lowercase()) else {
                return "Usage: /rollback <player name>".into();
            };
            let changes: Vec<u64> = cx
                .ledger
                .log
                .effective()
                .filter(|c| c.by.player_name.to_lowercase() == who || c.by.player.starts_with(&who))
                .map(|c| c.id)
                .collect();
            if changes.is_empty() {
                return format!("{who} has no changes in effect.");
            }
            let n = changes.len();
            cx.ledger.record(None, None, Action::Rollback { changes, note: format!("rollback of {who}") });
            rebuild(cx);
            format!("Rolled back {n} change(s) by {who}.")
        }
        "restore" => {
            let Some(id) = args.first().and_then(|n| n.parse::<u64>().ok()) else {
                return "Usage: /restore <change id>: the world goes back to how it was right after it".into();
            };
            let changes: Vec<u64> = cx.ledger.log.effective().filter(|c| c.id > id).map(|c| c.id).collect();
            if changes.is_empty() {
                return format!("Nothing has changed since change {id}.");
            }
            let n = changes.len();
            cx.ledger.record(None, None, Action::Rollback { changes, note: format!("restore to change {id}") });
            rebuild(cx);
            format!("Restored the world to change {id} ({n} later change(s) undone; /revert the rollback to bring them back).")
        }
        "land" => {
            let land = match args.first().copied() {
                Some("open") => LandMode::Open,
                Some("parcels") => LandMode::Parcels,
                _ => return "Usage: /land open|parcels".into(),
            };
            let mut rules = cx.ledger.rules.clone();
            rules.land = land;
            cx.ledger.record(None, None, Action::SetRules { rules });
            format!("Land is now {}.", if land == LandMode::Open { "open to everyone" } else { "claimed by parcels" })
        }
        "rate" => {
            let rates = match args.as_slice() {
                ["off"] => RateLimits::default(),
                [n, rest @ ..] => match (n.parse().ok(), rest.first().map(|v| v.parse().ok())) {
                    (Some(n), None) => RateLimits { changes_per_minute: Some(n), voxels_per_ten_minutes: None },
                    (Some(n), Some(Some(v))) => RateLimits { changes_per_minute: Some(n), voxels_per_ten_minutes: Some(v) },
                    _ => return "Usage: /rate <changes per minute> [voxels per 10 minutes], or /rate off".into(),
                },
                [] => return "Usage: /rate <changes per minute> [voxels per 10 minutes], or /rate off".into(),
            };
            let mut rules = cx.ledger.rules.clone();
            rules.rates = rates;
            cx.ledger.record(None, None, Action::SetRules { rules });
            format!("Rate limits set. {}", rate_line(&rates))
        }
        _ => HELP.into(),
    }
}

/// Makes the world (voxels and builds) match the change log again.
pub fn rebuild(cx: &mut Context) {
    let full = replay(&cx.ledger.log);
    for problem in &full.problems {
        warn!("change no longer applies: {problem}");
    }
    cx.world.replace_edits(full.edits);
    cx.creations.resync(cx.commands, &full.builds);
    cx.landscape.forget_history();
    cx.ledger.builds = full.builds;
}

fn history(ledger: &Ledger, n: usize) -> String {
    let undone = ledger.log.undone();
    let lines: Vec<String> = ledger
        .log
        .changes
        .iter()
        .rev()
        .take(n.clamp(1, 50))
        .map(|c| {
            let mark = if undone.contains(&c.id) { " (undone)" } else { "" };
            format!("{}: {} by {}{mark}", c.id, c.action.describe(), ledger.describe(&c.by))
        })
        .collect();
    if lines.is_empty() { "No changes yet.".into() } else { format!("Latest changes:\n{}", lines.join("\n")) }
}

fn rate_line(rates: &RateLimits) -> String {
    match (rates.changes_per_minute, rates.voxels_per_ten_minutes) {
        (None, None) => "No rate limits.".into(),
        (n, v) => format!(
            "At most {} changes a minute and {} voxels per 10 minutes (admins exempt).",
            n.map_or("unlimited".into(), |n| n.to_string()),
            v.map_or("unlimited".into(), |v| v.to_string())
        ),
    }
}

fn rules(ledger: &Ledger) -> String {
    let r = &ledger.rules;
    let land = match r.land {
        LandMode::Open => "open: anyone changes anything outside the spawn",
        LandMode::Parcels => "parcels: land is claimed, and owners change it",
    };
    format!(
        "Land is {land}. Spawn: {:.0} m around the start. One build: up to {} voxels a side, {} voxels, {} faces. \
         One land change: areas up to {:.0} m, {} voxels. {} Admins: {}.",
        r.spawn.radius_m,
        r.build.max_size,
        r.build.max_voxels,
        r.build.max_faces,
        r.terrain.max_area_m,
        r.terrain.max_changed_voxels,
        rate_line(&r.rates),
        ledger.log.admins.len()
    )
}
