//! Chat commands: `/name args` does the [op](super::ops) of that name, with the words
//! after it as its arguments. The chat suggests them as you type, and `/help` lists them.
//!
//! - `/history [n]`, `/undo <id>` (undoing an undo redoes), `/rules`, `/verify`.
//! - `/host`, `/join <host[:port]>`, `/leave`: play together. Talking to the other
//!   players needs no command: the chat's Game tab is theirs.
//! - `/save <name>`, `/saves`, `/fork <save> <world>`; admins `/load <name>`.
//! - `/worlds`, `/world <name>`, `/games`, `/game <name>`, `/newgame`, `/savegame`,
//!   `/portal`; admins `/newworld`, `/removeworld`, `/loadgame`.
//! - `/packages`; admins `/uninstall <name>`, `/rate`, `/bevel`.
//!
//! A command the game itself doesn't know goes to the world's game logic (`on_chat`).

use super::ops::{self, Args, Context};

/// Runs `/command args` and returns the reply for the chat.
pub fn run(text: &str, cx: &mut Context) -> String {
    let mut words = text.trim_start_matches('/').split_whitespace();
    let name = words.next().unwrap_or_default().to_lowercase();
    let words: Vec<&str> = words.collect();
    match ops::command(&name) {
        Some(op) => ops::said(ops::run_op(op, &Args::from_words(op.params, &words), cx)),
        // Not the game's own: the world's game logic may answer it (`on_chat`).
        None if !cx.ledger.logic.installed.is_empty() => {
            match cx.ledger.act(genverse_rules::logic::Intent::Chat {
                text: text.trim().to_string(),
                at: cx.at,
            }) {
                Ok(acted) if !acted.notes.is_empty() => acted.notes.join("\n"),
                Ok(_) => format!(
                    "The game didn't answer \"{}\". {}",
                    text.trim(),
                    ops::help()
                ),
                Err(why) => format!("⚠ {why}"),
            }
        }
        None => ops::help(),
    }
}
