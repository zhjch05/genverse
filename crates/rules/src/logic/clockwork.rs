//! Running timers live. Whoever keeps the world (a server; offline, the admin's game)
//! asks a [`Clockwork`] about twice a second which packages have timers due, fires them,
//! and records what they did as an [`crate::log::Action::Timers`]; replay applies that
//! and never runs a timer.
//!
//! A timer anchored to an entity ([`Timer::near`]) sleeps while no player is within
//! [`WAKE_RADIUS`] of it, so games nobody is near cost nothing. The clockwork remembers
//! when each sleeping timer last ran, so when someone comes back it goes off once, with
//! `missed` counting the runs it slept through.
//!
//! The engine's own timers (`~walk:2.0`, see [`super::walk`]) run here too, on the
//! ground the host knows, and are recorded like the package's.
//!
//! It's also where games see players: the host says who's in the world and where, and
//! each run the clockwork tells packages what changed since the last ([`Seen`]): who came
//! (`on_join`) or left (`on_leave`), who walked into or out of a piece's `region`
//! (`on_enter`, `on_exit`). Positions never go in the log; only what a game did about them
//! does, like a timer's. What it has seen lives only in memory, so after a restart
//! everyone there is greeted again.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use super::{
    Effect, EntityId, Event, GameState, Logic, LogicRules, Rules, Timer, Trigger,
    entity::{Entity, WAKE_RADIUS},
    grants,
    walk::{Engine, Ground, Heard, position_at},
};
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
    /// Who each package has seen come into the world.
    here: HashSet<(String, String)>,
    /// Who's inside each region, by its package and piece.
    inside: HashMap<(String, EntityId), BTreeSet<String>>,
}

/// Something that happened to a player, as the host saw it.
#[derive(Clone, Debug, PartialEq)]
pub enum Seen {
    Join(String),
    Leave(String),
    Enter(EntityId, String),
    Exit(EntityId, String),
}

/// The region taking players' keys (`keys = true`) that `at` is in, if any: a player
/// standing there on their pawn moves with `on_key`.
pub fn keys_region_at(state: &GameState, at: [f32; 3], now: u64) -> Option<EntityId> {
    state
        .entities()
        .find(|(_, e)| e.components.get("region").is_some_and(|r| r["keys"] == true) && in_region(e, at, now))
        .map(|(id, _)| id)
}

/// Whether `at` is inside `e`'s region, as it stands at `now`.
fn in_region(e: &Entity, at: [f32; 3], now: u64) -> bool {
    let Some(region) = e.components.get("region") else { return false };
    let c = position_at(e, now as f64);
    let (dx, dy, dz) = (at[0] - c[0], at[1] - c[1], at[2] - c[2]);
    // Regions reach a little above and below: a player on a step is still in.
    if dy.abs() > 4.0 {
        return false;
    }
    match (region["radius"].as_f64(), region["size"].as_array()) {
        (Some(r), _) => dx * dx + dz * dz <= (r * r) as f32,
        (None, Some(size)) => {
            let (w, d) = (size[0].as_f64().unwrap_or(0.0) as f32, size.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32);
            dx.abs() <= w / 2.0 && dz.abs() <= d / 2.0
        }
        _ => false,
    }
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
        Self { since, ..Default::default() }
    }

    pub fn since(&self) -> u64 {
        self.since
    }

    /// The world is paused: what comes due by `now` is skipped, not saved up, so
    /// resuming doesn't fire a pause's worth of timers at once. A one-time timer that
    /// came due goes off when the world resumes.
    pub fn hold(&mut self, now: u64) {
        self.since = self.since.max(now);
        for last in self.held.values_mut() {
            *last = (*last).max(now);
        }
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
        ground: &dyn Ground,
        package: &str,
        change: u64,
        now: u64,
        world_seed: u32,
        rules: &LogicRules,
        players: &[(String, [f32; 3])],
        seen: &[Seen],
    ) -> Fired {
        let mut out = Fired::default();
        let positions: Vec<[f32; 3]> = players.iter().map(|(_, p)| *p).collect();
        let players_at = players;
        let players = positions.as_slice();
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
        // What the other packages told this one since it last heard (their `emit`).
        let unheard: Vec<_> = logic.state.unheard(package).cloned().collect();
        for (i, signal) in unheard.iter().enumerate() {
            let seed = fnv1a(format!("{world_seed}/{change}/{package}/signal/{i}").as_bytes());
            let trigger = Trigger::Signal { from: &signal.from, name: &signal.name, data: &signal.data };
            let event = Event { ground: Some(ground), change, player: "", trigger, time: logic.time(rules, now), at: now, seed };
            let mark = batch.mark();
            if let Err(why) = runtime.handle(found, &event, &mut batch, &grants) {
                batch.rollback(mark);
                out.errors.push(format!("{package}: on_signal \"{}\": {why}", signal.name));
            }
        }
        if let Some(last) = unheard.last() {
            let _ = batch.apply(&grants, &Effect::Heard { upto: last.change });
        }
        // What players did first: they came, left, walked into regions.
        for (i, what) in seen.iter().enumerate() {
            let (player, trigger) = match what {
                Seen::Join(p) => (p, Trigger::Join),
                Seen::Leave(p) => (p, Trigger::Leave),
                Seen::Enter(entity, p) => (p, Trigger::Enter { entity: *entity }),
                Seen::Exit(entity, p) => (p, Trigger::Exit { entity: *entity }),
            };
            let seed = fnv1a(format!("{world_seed}/{change}/{package}/seen/{i}").as_bytes());
            let event = Event { ground: Some(ground), change, player, trigger, time: logic.time(rules, now), at: now, seed };
            let mark = batch.mark();
            if let Err(why) = runtime.handle(found, &event, &mut batch, &grants) {
                batch.rollback(mark);
                out.errors.push(format!("{package}: {}: {why}", handler_of(what)));
            }
        }
        for (_, name, timer, n) in &due {
            self.held.remove(&(package.to_string(), name.clone()));
            if timer.every.is_none() {
                let _ = batch.apply(&grants, &Effect::Cancel { name: name.clone() });
            }
            let seed = fnv1a(format!("{world_seed}/{change}/{package}/{name}").as_bytes());
            // The engine's own: walking and chasing. A piece that arrives tells
            // its package.
            if let Some(engine) = name.strip_prefix('~') {
                let mark = batch.mark();
                let mut cx = Engine { batch: &mut batch, grants: &grants, ground, now, players: players_at };
                match cx.fire(engine) {
                    Ok(Some(heard)) => {
                        let (player, trigger, handler) = match &heard {
                            Heard::Arrived { entity, stuck } => ("", Trigger::Arrive { entity: *entity, stuck: *stuck }, "on_arrive"),
                            Heard::Caught { entity, player } => (player.as_str(), Trigger::Catch { entity: *entity }, "on_catch"),
                        };
                        let event = Event { ground: Some(ground), change, player, trigger, time: logic.time(rules, now), at: now, seed };
                        let mark = batch.mark();
                        if let Err(why) = runtime.handle(found, &event, &mut batch, &grants) {
                            batch.rollback(mark);
                            out.errors.push(format!("{package}: {handler}: {why}"));
                        }
                    }
                    Ok(None) => {}
                    Err(why) => {
                        batch.rollback(mark);
                        out.errors.push(format!("{package}: {name}: {why}"));
                        let _ = batch.apply(&grants, &Effect::Cancel { name: name.clone() });
                    }
                }
                continue;
            }
            let trigger = Trigger::Timer { name, data: &timer.data, missed: n - 1 };
            let event = Event { ground: Some(ground), change, player: "", trigger, time: logic.time(rules, now), at: now, seed };
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
        let live: HashSet<(&str, &str)> = logic.state.timers().map(|(p, n, _)| (p, n)).collect();
        self.held.retain(|(p, n), _| live.contains(&(p.as_str(), n.as_str())));
        // Regions whose piece is gone, and packages that are gone, forget who they saw.
        self.inside.retain(|(p, id), _| logic.state.get(*id).is_some_and(|e| &e.package == p && e.components.contains_key("region")));
        self.here.retain(|(p, _)| logic.installed.contains_key(p));
        self.since = self.since.max(now);
    }

    /// Fires every package's due timers up to `now`, one package at a time, and has
    /// `host` record and apply what each one's did (as an
    /// [`crate::log::Action::Timers`] by the world), so the next sees it.
    pub fn run(&mut self, host: &mut dyn TimerHost, now: u64, world_seed: u32, rules: &LogicRules, players: &[(String, [f32; 3])]) -> Run {
        let mut out = Run::default();
        let positions: Vec<[f32; 3]> = players.iter().map(|(_, p)| *p).collect();
        let mut packages: BTreeSet<String> = self.due(host.logic(), now, &positions).into_iter().collect();
        // Packages with signals to hear.
        let logic = host.logic();
        packages.extend(logic.installed.keys().filter(|p| logic.state.unheard(p).next().is_some()).cloned());
        let mut seen: HashMap<String, Vec<Seen>> = HashMap::new();
        for package in host.logic().installed.keys() {
            let saw = self.watch(host.logic(), package, players, now);
            if !saw.is_empty() {
                packages.insert(package.clone());
                seen.insert(package.clone(), saw);
            }
        }
        if packages.is_empty() && now <= self.since && self.held.is_empty() {
            return out;
        }
        for package in packages {
            let saw = seen.remove(&package).unwrap_or_default();
            let change = host.next_change();
            let fired = self.fire(host.logic(), &*host, &package, change, now, world_seed, rules, players, &saw);
            self.note(&package, &saw);
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
        self.finish(host.logic(), now, &positions);
        out
    }

    /// What `package` hasn't heard yet: who came and left, who walked into or out of its
    /// regions.
    pub fn watch(&self, logic: &Logic, package: &str, players: &[(String, [f32; 3])], now: u64) -> Vec<Seen> {
        let mut out = Vec::new();
        for (player, _) in players {
            if !self.here.contains(&(package.to_string(), player.clone())) {
                out.push(Seen::Join(player.clone()));
            }
        }
        for (p, player) in &self.here {
            if p == package && !players.iter().any(|(q, _)| q == player) {
                out.push(Seen::Leave(player.clone()));
            }
        }
        let empty = BTreeSet::new();
        for (id, e) in logic.state.entities().filter(|(_, e)| e.package == package && e.components.contains_key("region")) {
            let was = self.inside.get(&(package.to_string(), id)).unwrap_or(&empty);
            for (player, at) in players {
                let now_in = in_region(e, *at, now);
                if now_in && !was.contains(player) {
                    out.push(Seen::Enter(id, player.clone()));
                }
                if !now_in && was.contains(player) {
                    out.push(Seen::Exit(id, player.clone()));
                }
            }
            for player in was {
                if !players.iter().any(|(q, _)| q == player) {
                    out.push(Seen::Exit(id, player.clone()));
                }
            }
        }
        out.sort_by_key(|s| matches!(s, Seen::Leave(_) | Seen::Exit(..)));
        out
    }

    /// `package` has heard about `seen`.
    pub fn note(&mut self, package: &str, seen: &[Seen]) {
        for what in seen {
            match what {
                Seen::Join(p) => {
                    self.here.insert((package.to_string(), p.clone()));
                }
                Seen::Leave(p) => {
                    self.here.remove(&(package.to_string(), p.clone()));
                }
                Seen::Enter(id, p) => {
                    self.inside.entry((package.to_string(), *id)).or_default().insert(p.clone());
                }
                Seen::Exit(id, p) => {
                    if let Some(inside) = self.inside.get_mut(&(package.to_string(), *id)) {
                        inside.remove(p);
                    }
                }
            }
        }
    }
}

/// What keeps a world while its timers run: a server, the game offline, a trial. It
/// knows the ground, for pieces that walk.
pub trait TimerHost: Ground {
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

fn handler_of(seen: &Seen) -> &'static str {
    match seen {
        Seen::Join(_) => "on_join",
        Seen::Leave(_) => "on_leave",
        Seen::Enter(..) => "on_enter",
        Seen::Exit(..) => "on_exit",
    }
}
