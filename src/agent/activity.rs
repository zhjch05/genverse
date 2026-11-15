//! The live feed in the chat while an agent works: what it says and each thing it reads,
//! edits and runs, as they happen. It folds into one line when the work is over.

use std::time::Instant;

use bevy_egui::egui;
use egui_phosphor::regular as icon;

use super::{
    stream::{Activity, Kind},
    theme,
};

/// The most steps shown at once; earlier ones are counted, not listed.
const SHOWN: usize = 8;

enum Step {
    Note(String),
    /// `done` is how it went, once it's over.
    Tool {
        id: String,
        kind: Kind,
        what: String,
        done: Option<bool>,
    },
}

pub struct Feed {
    started: Instant,
    steps: Vec<Step>,
    thinking: bool,
}

impl Default for Feed {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            steps: Vec::new(),
            thinking: false,
        }
    }
}

impl Feed {
    pub fn apply(&mut self, activity: Activity) {
        self.thinking = matches!(activity, Activity::Thinking);
        match activity {
            Activity::Thinking => {}
            Activity::Note(text) => self.steps.push(Step::Note(text)),
            Activity::NoteMore(more) => match self.steps.last_mut() {
                Some(Step::Note(text)) => text.push_str(&more),
                _ => self.steps.push(Step::Note(more)),
            },
            Activity::Tool { id, kind, what } => self.steps.push(Step::Tool {
                id,
                kind,
                what,
                done: None,
            }),
            Activity::ToolDone { id, ok } => {
                let call = self.steps.iter_mut().rev().find_map(|step| match step {
                    Step::Tool { id: this, done, .. } if *this == id => Some(done),
                    _ => None,
                });
                if let Some(done) = call {
                    *done = Some(ok);
                }
            }
        }
    }

    fn tools(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| matches!(step, Step::Tool { .. }))
            .count()
    }

    /// What's left in the chat afterwards ("Worked 42s · 6 steps"), if it did anything
    /// worth a line.
    pub fn summary(&self) -> Option<String> {
        let tools = self.tools();
        let steps = if tools == 1 { "step" } else { "steps" };
        (tools > 0).then(|| format!("Worked {} · {tools} {steps}", elapsed(self.started)))
    }

    /// Draws the feed as a message from the agent that's still being written.
    pub fn show(&self, ui: &mut egui::Ui, name: &str, color: egui::Color32) {
        let p = theme::palette(ui.ctx());
        ui.horizontal_top(|ui| {
            ui.add_space(12.0);
            theme::avatar(ui, name, color, 36.0);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.set_max_width(ui.available_width() - 12.0);
                ui.spacing_mut().item_spacing.y = 3.0;
                ui.horizontal(|ui| {
                    let readable = if ui.visuals().dark_mode {
                        color
                    } else {
                        theme::shade(color, 0.62)
                    };
                    ui.label(egui::RichText::new(name).strong().color(readable));
                    theme::tag(ui, "AGENT", p.primary, p.on_primary);
                    ui.label(
                        egui::RichText::new(format!("working · {}", elapsed(self.started)))
                            .size(11.0)
                            .color(p.faint),
                    );
                });
                let hidden = self.steps.len().saturating_sub(SHOWN);
                if hidden > 0 {
                    let steps = if hidden == 1 { "step" } else { "steps" };
                    ui.label(
                        egui::RichText::new(format!("{hidden} earlier {steps}"))
                            .size(11.5)
                            .color(p.faint),
                    );
                }
                let mut running = false;
                for step in &self.steps[hidden..] {
                    match step {
                        Step::Note(text) => {
                            ui.label(egui::RichText::new(text.trim()).color(p.text));
                        }
                        Step::Tool {
                            kind, what, done, ..
                        } => {
                            running |= done.is_none();
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                let glyph = match kind {
                                    Kind::Look => icon::MAGNIFYING_GLASS,
                                    Kind::Edit => icon::PENCIL_SIMPLE,
                                    Kind::World => icon::CUBE,
                                    Kind::Run => icon::TERMINAL_WINDOW,
                                };
                                ui.label(egui::RichText::new(glyph).size(13.0).color(p.muted));
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(what).size(12.5).color(p.muted),
                                    )
                                    .truncate(),
                                );
                                match done {
                                    None => {
                                        drop(ui.add(egui::Spinner::new().size(10.0).color(p.muted)))
                                    }
                                    Some(true) => drop(ui.label(
                                        egui::RichText::new(icon::CHECK).size(11.0).color(p.faint),
                                    )),
                                    Some(false) => drop(ui.label(
                                        egui::RichText::new(icon::X).size(11.0).color(p.danger),
                                    )),
                                }
                            });
                        }
                    }
                }
                // Between steps there's still a sign of life.
                if !running {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        ui.add(egui::Spinner::new().size(10.0).color(p.faint));
                        let doing = if self.thinking {
                            "thinking…"
                        } else {
                            "working…"
                        };
                        ui.label(egui::RichText::new(doing).size(11.5).color(p.faint));
                    });
                }
            });
        });
    }
}

/// "42s", "3m 05s".
fn elapsed(since: Instant) -> String {
    match since.elapsed().as_secs() {
        s if s < 60 => format!("{s}s"),
        s => format!("{}m {:02}s", s / 60, s % 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_grow_and_calls_finish_by_id() {
        let mut feed = Feed::default();
        assert_eq!(feed.summary(), None);
        feed.apply(Activity::Note("I'll read".into()));
        feed.apply(Activity::NoteMore(" the rules.".into()));
        feed.apply(Activity::Tool {
            id: "a".into(),
            kind: Kind::Look,
            what: "README.md".into(),
        });
        feed.apply(Activity::Tool {
            id: "b".into(),
            kind: Kind::Edit,
            what: "wolves.luau".into(),
        });
        feed.apply(Activity::ToolDone {
            id: "a".into(),
            ok: true,
        });
        feed.apply(Activity::ToolDone {
            id: "b".into(),
            ok: false,
        });
        assert!(matches!(&feed.steps[0], Step::Note(text) if text == "I'll read the rules."));
        assert!(matches!(
            &feed.steps[1],
            Step::Tool {
                done: Some(true),
                ..
            }
        ));
        assert!(matches!(
            &feed.steps[2],
            Step::Tool {
                done: Some(false),
                ..
            }
        ));
        assert_eq!(feed.summary().as_deref(), Some("Worked 0s · 2 steps"));
    }
}
