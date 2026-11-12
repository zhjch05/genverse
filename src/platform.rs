//! Where the game keeps its files, and finding the agent CLIs, when it's started outside a
//! terminal (a downloaded build opened from Finder, Explorer or a desktop launcher).
//!
//! - **Files:** saves, settings and the agent's workspaces are relative paths (`saves/…`).
//!   Run from a checkout (`cargo run`, or a folder that already has `saves/`) they stay
//!   there. Otherwise the game moves into the platform's data folder first, so a release
//!   build never writes beside the executable or into `/`. `GENVERSE_HOME` picks the folder.
//! - **Agents:** an app opened from the desktop gets a short `PATH` that rarely has
//!   `claude` or `codex` (or the `node` they need), so on macOS and Linux the login
//!   shell's `PATH` is added. [`find_program`] also follows Windows' `PATHEXT`, which a
//!   bare `Command::new("codex")` doesn't.

use std::path::{Path, PathBuf};

/// Runs once, before anything reads or writes a file or starts a process.
pub fn prepare() {
    let home = data_home();
    if let Some(home) = &home
        && let Err(e) = std::fs::create_dir_all(home).and_then(|_| std::env::set_current_dir(home))
    {
        eprintln!("genverse: can't use {} for saves: {e}", home.display());
    }
    // From a terminal the PATH is already the user's own.
    #[cfg(unix)]
    if home.is_some() {
        add_login_shell_path();
    }
}

/// The folder to keep files in, or None to stay in the current one.
fn data_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("GENVERSE_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    // `cargo run` (and `bevy run`) sets this; so does a checkout with saves in it.
    if std::env::var_os("CARGO_MANIFEST_DIR").is_some() || Path::new("saves").is_dir() {
        return None;
    }
    platform_data_dir()
}

fn platform_data_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        var("APPDATA").map(|d| d.join("Genverse"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support/Genverse"))
    } else {
        var("XDG_DATA_HOME").or_else(|| var("HOME").map(|h| h.join(".local/share"))).map(|d| d.join("genverse"))
    }
}

/// Adds the directories of the login shell's `PATH` (where installers put `claude`,
/// `codex` and `node`) that ours lacks, plus the usual install folders.
#[cfg(unix)]
fn add_login_shell_path() {
    use std::{process::Command, time::Duration};
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let mut add = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    // An interactive login shell reads the files version managers (nvm, fnm, asdf) hook
    // into. Markers keep out whatever its startup files print.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    if let Ok(mut child) = Command::new(&shell)
        .args(["-ilc", "printf '\\n__GV__%s__GV__\\n' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // A shell whose startup files wait for input mustn't hold up the game.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            if let Ok(out) = child.wait_with_output() {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(path) = text.split("__GV__").nth(1) {
                    std::env::split_paths(path).for_each(&mut add);
                }
            }
        } else {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir in [".local/bin", ".claude/local", ".npm-global/bin", ".volta/bin", ".bun/bin"] {
            add(home.join(dir));
        }
    }
    for dir in ["/opt/homebrew/bin", "/usr/local/bin"] {
        add(PathBuf::from(dir));
    }
    if let Ok(path) = std::env::join_paths(dirs) {
        // SAFETY: called from `main` before the app, or any other thread, starts.
        unsafe { std::env::set_var("PATH", path) };
    }
}

/// The full path of a program on `PATH`, trying Windows' executable extensions.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        // A native `.exe` first: npm's `.cmd` shims run through cmd.exe, which mangles
        // long arguments.
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut exts: Vec<String> = pathext.split(';').filter(|e| !e.is_empty()).map(str::to_ascii_lowercase).collect();
        exts.sort_by_key(|e| e != ".exe");
        exts
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions.iter().map(|ext| dir.join(format!("{name}{ext}"))).find(|candidate| is_program(candidate))
    })
}

fn is_program(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_program_everyone_has() {
        let name = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(find_program(name).is_some_and(|p| p.is_absolute()));
        assert!(find_program("genverse-surely-not-installed").is_none());
    }
}
