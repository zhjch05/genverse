//! The game's chat: what players say to everyone in the game, whichever world they're
//! in. The latest [`CHAT_KEPT`] messages are kept beside the game (`chat.jsonl`), not in
//! any log: saves, undo and history don't touch them, and a player who joins is sent
//! them.

use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use genverse_net::{CHAT_KEPT, Said};

/// The most messages a player may send within [`CHAT_WINDOW`].
pub const CHAT_BURST: usize = 8;
pub const CHAT_WINDOW: Duration = Duration::from_secs(10);

pub struct Chat {
    path: PathBuf,
    kept: VecDeque<Said>,
    /// Lines in the file: it's rewritten with only the kept ones once it's twice that.
    lines: usize,
}

impl Chat {
    /// The chat kept in `dir`, if any.
    pub fn open(dir: &Path) -> io::Result<Self> {
        let path = dir.join("chat.jsonl");
        let mut kept = VecDeque::new();
        let mut lines = 0;
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines() {
                lines += 1;
                // A torn last line (the server stopped mid-write) is skipped.
                if let Ok(said) = serde_json::from_str::<Said>(&line?) {
                    kept.push_back(said);
                    if kept.len() > CHAT_KEPT {
                        kept.pop_front();
                    }
                }
            }
        }
        let mut chat = Self { path, kept, lines };
        if chat.lines > chat.kept.len() {
            chat.rewrite()?;
        }
        Ok(chat)
    }

    /// The kept messages, oldest first.
    pub fn messages(&self) -> Vec<Said> {
        self.kept.iter().cloned().collect()
    }

    /// Keeps a message said to everyone.
    pub fn keep(&mut self, said: Said) -> io::Result<()> {
        let line = serde_json::to_string(&said)?;
        self.kept.push_back(said);
        if self.kept.len() > CHAT_KEPT {
            self.kept.pop_front();
        }
        if self.lines + 1 >= 2 * CHAT_KEPT {
            return self.rewrite();
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        self.lines += 1;
        Ok(())
    }

    /// Writes only the kept messages, through a temporary file so a crash loses nothing.
    fn rewrite(&mut self) -> io::Result<()> {
        let temp = self.path.with_extension("jsonl.tmp");
        let mut file = File::create(&temp)?;
        for said in &self.kept {
            writeln!(file, "{}", serde_json::to_string(said)?)?;
        }
        file.sync_all()?;
        std::fs::rename(&temp, &self.path)?;
        self.lines = self.kept.len();
        Ok(())
    }
}

/// Whether a player who sent at the times in `sent` may send again now; if so, notes it.
pub fn allowed(sent: &mut VecDeque<Instant>, now: Instant) -> bool {
    while sent
        .front()
        .is_some_and(|t| now.duration_since(*t) >= CHAT_WINDOW)
    {
        sent.pop_front();
    }
    if sent.len() >= CHAT_BURST {
        return false;
    }
    sent.push_back(now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(text: &str) -> Said {
        Said {
            from: "p".into(),
            name: "P".into(),
            world: "main".into(),
            text: text.into(),
            at: 1,
        }
    }

    #[test]
    fn keeps_the_latest_and_reopens_them() {
        let dir = std::env::temp_dir().join(format!("genverse-chat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut chat = Chat::open(&dir).unwrap();
        for n in 0..CHAT_KEPT * 3 {
            chat.keep(said(&n.to_string())).unwrap();
        }
        let messages = chat.messages();
        assert_eq!(messages.len(), CHAT_KEPT);
        assert_eq!(messages[0].text, (CHAT_KEPT * 2).to_string());
        // The file never grows past twice what's kept.
        let lines = std::fs::read_to_string(dir.join("chat.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert!(lines < 2 * CHAT_KEPT, "{lines}");
        // A torn line is skipped, and the rest come back.
        let mut file = OpenOptions::new()
            .append(true)
            .open(dir.join("chat.jsonl"))
            .unwrap();
        write!(file, "{{\"from\":").unwrap();
        assert_eq!(Chat::open(&dir).unwrap().messages(), messages);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_burst_is_allowed_then_it_waits() {
        let mut sent = VecDeque::new();
        let start = Instant::now();
        for _ in 0..CHAT_BURST {
            assert!(allowed(&mut sent, start));
        }
        assert!(!allowed(&mut sent, start + Duration::from_secs(1)));
        assert!(allowed(&mut sent, start + CHAT_WINDOW));
    }
}
