//! Running timers live. Whoever keeps the world (a server; offline, the admin's game)
//! asks a [`Clockwork`] about twice a second which packages have timers due, fires them,
//! and records what they did as an [`crate::log::Action::Timers`]; replay applies that
//! and never runs a timer.
//!
//! A timer anchored to an entity ([`Timer::near`]) sleeps while no player is within
//! [`WAKE_RADIUS`] of it, so games nobody is near cost nothing. The clockwork remembers
//! when each sleeping timer last ran, so when someone comes back it goes off once, with
//! `missed` counting the runs it slept through.

use std::{collections::HashMap, sync::Arc};

use super::{Effect, Event, GameState, Logic, LogicRules, Rules, Timer, Trigger, entity::WAKE_RADIUS, grants};
use crate::log::fnv1a;

/// The most timers one package runs per firing; the rest wait for the next.
pub const MAX_FIRED: usize = 64;

/// What a package's timers did: the effects to record (a one-time timer that went off
/// records cancelling itself, so replay forgets it too), and those that failed (which
/// cancel themselves).
#[derive(Debug, Default)]
pub struct Fired {
    pub effects: Vec<Effect>,
    pub notes: Vec<(String, String)>,
    pub errors: Vec<String>,
}

impl Fired {
    pub fn worth_recording(&self) -> bool {
        !self.effects.is_empty()
    }
}

/// Which timers have run up to when. Lives only in memory: after a restart it starts
/// from the log's [`Logic::timers_until`].
#[derive(Clone, Debug, Default)]
pub struct Clockwork {
    /// Awake timers have run up to here.
    since: u64,
    /// Sleeping (and held back) timers: when they last ran, by package and name.
    held: HashMap<(String, String), u64>,
}

fn distance2(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

/// Whether `timer` runs with players at `players`: unanchored ones always do, and so
/// does one whose entity is gone (so its script can notice).
pub fn awake(timer: &Timer, state: &GameState, players: &[[f32; 3]]) -> bool {
    let Some(id) = timer.near else { return true };
    let Some(e) = state.get(id) else { return true };
    players.iter().any(|p| distance2(*p, e.position) <= WAKE_RADIUS * WAKE_RADIUS)
}

impl Clockwork {
    /// Starting after `since` (Unix seconds): what [`Logic::timers_until`] says, after a
    /// load.
    pub fn new(since: u64) -> Self {
        Self { since, held: HashMap::new() }
    }

    pub fn since(&self) -> u64 {
        self.since
    }

    fn window(&self, package: &str, name: &str) -> u64 {
        self.held.get(&(package.to_string(), name.to_string())).copied().unwrap_or(self.since)
    }

    /// Installed packages with an awake timer due by `now`.
    pub fn due(&self, logic: &Logic, now: u64, players: &[[f32; 3]]) -> Vec<String> {
        let mut due: Vec<String> = logic
            .state
            .timers()
            .filter(|(p, n, t)| {
                logic.installed.contains_key(*p) && t.fires(self.window(p, n), now).is_some() && awake(t, &logic.state, players)
            })
            .map(|(p, _, _)| p.to_string())
            .collect();
        due.dedup();
        due
    }

    /// When the next awake timer goes off after the last run, if any.
    pub fn next_due(&self, logic: &Logic, players: &[[f32; 3]]) -> Option<u64> {
        logic
            .state
            .timers()
            .filter(|(p, _, t)| logic.installed.contains_key(*p) && awake(t, &logic.state, players))
            .filter_map(|(p, n, t)| t.next(self.window(p, n)))
            .min()
    }

    /// Runs `package`'s awake timers due by `now`, as change `change`, without changing
    /// `logic`: the caller records [`Fired::effects`] and applies them
    /// ([`Logic::apply_timers`]). A timer that fails is undone and cancelled, and the
    /// others still run.
    #[allow(clippy::too_many_arguments)]
    pub fn fire(
        &mut self,
        logic: &Logic,
        package: &str,
        change: u64,
        now: u64,
        world_seed: u32,
        rules: &LogicRules,
        players: &[[f32; 3]],
    ) -> Fired {
        let mut out = Fired::default();
        let Some(install) = logic.installed.get(package) else { return out };
        let Some(found) = logic.package(install) else { return out };
        let runtime = logic.runtime.clone().unwrap_or_else(|| Arc::new(Rules));
        let grants = grants(found, install, rules);
        let mut due: Vec<(u64, String, Timer, u64)> = logic
            .state
            .timers_of(package)
            .filter(|(_, t)| awake(t, &logic.state, players))
            .filter_map(|(name, t)| t.fires(self.window(package, name), now).map(|(at, n)| (at, name.to_string(), t.clone(), n)))
            .collect();
        due.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        // The rest keep their place for next time.
        for (_, name, _, _) in due.iter().skip(MAX_FIRED) {
            let key = (package.to_string(), name.clone());
            let since = self.window(package, name);
            self.held.insert(key, since);
        }
        due.truncate(MAX_FIRED);
        let mut batch = logic.state.batch(change, "", rules.max_effects);
        for (_, name, timer, n) in &due {
            self.held.remove(&(package.to_string(), name.clone()));
            if timer.every.is_none() {
                let _ = batch.apply(&grants, &Effect::Cancel { name: name.clone() });
            }
            let seed = fnv1a(format!("{world_seed}/{change}/{package}/{name}").as_bytes());
            let trigger = Trigger::Timer { name, data: &timer.data, missed: n - 1 };
            let event = Event { change, player: "", trigger, time: rules.clock.at(now), at: now, seed };
            let mark = batch.mark();
            if let Err(why) = runtime.handle(found, &event, &mut batch, &grants) {
                batch.rollback(mark);
                out.errors.push(format!("{package}: timer \"{name}\" stopped: {why}"));
                let _ = batch.apply(&grants, &Effect::Cancel { name: name.clone() });
            }
        }
        out.effects = std::mem::take(&mut batch.effects);
        out.notes = batch.finish().notes;
        out
    }

    /// Done with the run up to `now`: timers asleep now remember when they last ran,
    /// and those that woke up forget it.
    pub fn finish(&mut self, logic: &Logic, now: u64, players: &[[f32; 3]]) {
        let since = self.since;
        for (p, n, t) in logic.state.timers() {
            let key = (p.to_string(), n.to_string());
            if !awake(t, &logic.state, players) {
                self.held.entry(key).or_insert(since);
            }
        }
        let live: std::collections::HashSet<(&str, &str)> = logic.state.timers().map(|(p, n, _)| (p, n)).collect();
        self.held.retain(|(p, n), _| live.contains(&(p.as_str(), n.as_str())));
        self.since = self.since.max(now);
    }

    /// Fires every package's due timers up to `now`, one package at a time, and has
    /// `host` record and apply what each one's did (as an
    /// [`crate::log::Action::Timers`] by the world), so the next sees it.
    pub fn run(&mut self, host: &mut dyn TimerHost, now: u64, world_seed: u32, rules: &LogicRules, players: &[[f32; 3]]) -> Run {
        let mut out = Run::default();
        if now <= self.since && self.held.is_empty() {
            return out;
        }
        for package in self.due(host.logic(), now, players) {
            let change = host.next_change();
            let fired = self.fire(host.logic(), &package, change, now, world_seed, rules, players);
            out.errors.extend(fired.errors.iter().cloned());
            if !fired.worth_recording() {
                continue;
            }
            match host.record(&package, now, fired.effects) {
                Ok(()) => {
                    out.notes.extend(fired.notes);
                    out.changes += 1;
                }
                Err(why) => out.errors.push(format!("{package}: {why}")),
            }
        }
        self.finish(host.logic(), now, players);
        out
    }
}

/// What keeps a world while its timers run: a server, the game offline, a trial.
pub trait TimerHost {
    fn logic(&self) -> &Logic;
    /// The id the next change will get.
    fn next_change(&self) -> u64;
    /// Records what `package`'s timers did by `now`, by the world, and applies it as
    /// replay would (its builds and land changes too), or says why it can't.
    fn record(&mut self, package: &str, now: u64, effects: Vec<Effect>) -> Result<(), String>;
}

/// What a [`Clockwork::run`] did.
#[derive(Debug, Default)]
pub struct Run {
    /// How many changes it recorded.
    pub changes: usize,
    /// Messages for players: who, and what.
    pub notes: Vec<(String, String)>,
    pub errors: Vec<String>,
}
