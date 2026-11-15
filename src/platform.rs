//! Where the game keeps its files, and finding the agent CLIs, when it's started outside a
//! terminal (a downloaded build opened from Finder, Explorer or a desktop launcher).
//!
//! - **Files:** saves, settings and the agent's workspaces are relative paths (`saves/…`).
//!   Run from a checkout (`cargo run`, or a folder that already has `saves/`) they stay
//!   there. Otherwise the game moves into the platform's data folder first, so a release
//!   build never writes beside the executable or into `/`. `GENVERSE_HOME` picks the folder.
//! - **Agents:** an app opened from the desktop gets a short `PATH` that rarely has
//!   `claude` or `codex` (or the `node` they need), so on macOS and Linux the login
//!   shell's `PATH` is added, with the variables the agents' providers read their keys
//!   and endpoints from (an `OPENAI_API_KEY` or `ANTHROPIC_BASE_URL` exported in
//!   `.zshrc`, say) that ours lacks. [`find_program`] also follows Windows' `PATHEXT`, which a
//!   bare `Command::new("codex")` doesn't, and [`command`] runs npm's `.cmd` shims
//!   through `node` directly, since Windows can't pass a prompt to a batch file.

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
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        var("APPDATA").map(|d| d.join("Genverse"))
    } else if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library/Application Support/Genverse"))
    } else {
        var("XDG_DATA_HOME")
            .or_else(|| var("HOME").map(|h| h.join(".local/share")))
            .map(|d| d.join("genverse"))
    }
}

/// Adds the directories of the login shell's `PATH` (where installers put `claude`,
/// `codex` and `node`) that ours lacks, plus the usual install folders.
#[cfg(unix)]
fn add_login_shell_path() {
    use std::{process::Command, time::Duration};
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut add = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    // An interactive login shell reads the files version managers (nvm, fnm, asdf) hook
    // into. Markers keep out whatever its startup files print.
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    if let Ok(mut child) = Command::new(&shell)
        .args(["-ilc", "printf '\\n__GV__%s__GV__\\n' \"$PATH\"; env -0"])
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
                let mut parts = text.splitn(3, "__GV__").skip(1);
                if let Some(path) = parts.next() {
                    std::env::split_paths(path).for_each(&mut add);
                }
                if let Some(env) = parts.next() {
                    add_provider_env(env.trim_start_matches('\n'));
                }
            }
        } else {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for dir in [
            ".local/bin",
            ".claude/local",
            ".npm-global/bin",
            ".volta/bin",
            ".bun/bin",
        ] {
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

/// Sets the variables in `env` (`env -0`'s output) that the agents' providers read and
/// that aren't set already.
#[cfg(unix)]
fn add_provider_env(env: &str) {
    let wanted = crate::agent::config::wanted_env();
    for line in env.split('\0') {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if wanted(name) && std::env::var_os(name).is_none() {
            // SAFETY: called from `main` before the app, or any other thread, starts.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

/// The full path of a program on `PATH`, trying Windows' executable extensions.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        // A native `.exe` first: npm's `.cmd` shims run through cmd.exe, which mangles
        // long arguments.
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut exts: Vec<String> = pathext
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        exts.sort_by_key(|e| e != ".exe");
        exts
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        extensions
            .iter()
            .map(|ext| dir.join(format!("{name}{ext}")))
            .find(|candidate| is_program(candidate))
    })
}

/// A command that runs `program`. An npm-installed CLI on Windows is a `.cmd` shim, and
/// Rust refuses to give a batch file arguments that cmd.exe can't quote safely (any
/// newline or quote: every prompt), so the shim's script is run with `node` instead.
pub fn command(program: &Path) -> std::process::Command {
    let is_batch = program
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if is_batch
        && let Some(dir) = program.parent()
        && let Ok(text) = std::fs::read_to_string(program)
        && let Some(script) = shim_script(&text, dir)
        && script.is_file()
    {
        let bundled = dir.join("node.exe");
        let node = if bundled.is_file() {
            Some(bundled)
        } else {
            find_program("node")
        };
        if let Some(node) = node {
            let mut cmd = std::process::Command::new(node);
            cmd.arg(script);
            return cmd;
        }
    }
    std::process::Command::new(program)
}

/// The script an npm `.cmd` shim runs: the quoted path after `%dp0%` (or `%~dp0`), which
/// stands for the shim's own folder.
fn shim_script(text: &str, dir: &Path) -> Option<PathBuf> {
    // Older shims name `"%~dp0\node.exe"` on the same line, so take the last match that
    // isn't node itself.
    let relative = ["\"%dp0%\\", "\"%~dp0\\"]
        .iter()
        .flat_map(|start| {
            text.match_indices(start)
                .map(move |(at, _)| at + start.len())
        })
        .filter_map(|from| text[from..].find('"').map(|to| &text[from..from + to]))
        .filter(|path| !path.eq_ignore_ascii_case("node.exe"))
        .max_by_key(|path| text.find(path))?;
    let mut path = dir.to_path_buf();
    path.extend(relative.split(['\\', '/']).filter(|part| !part.is_empty()));
    Some(path)
}

fn is_program(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
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

    #[test]
    fn reads_the_script_out_of_an_npm_shim() {
        let dir = Path::new("npm");
        let now = "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*";
        let old =
            "  \"%~dp0\\node.exe\"  \"%~dp0\\node_modules\\@anthropic-ai\\claude-code\\cli.js\" %*";
        let expect = |parts: &[&str]| {
            parts
                .iter()
                .fold(dir.to_path_buf(), |path, part| path.join(part))
        };
        assert_eq!(
            shim_script(&format!("@ECHO off\n{now}\n"), dir),
            Some(expect(&[
                "node_modules",
                "@openai",
                "codex",
                "bin",
                "codex.js"
            ]))
        );
        assert_eq!(
            shim_script(old, dir),
            Some(expect(&[
                "node_modules",
                "@anthropic-ai",
                "claude-code",
                "cli.js"
            ]))
        );
        assert_eq!(shim_script("@ECHO off\ncodex.exe %*", dir), None);
    }
}
