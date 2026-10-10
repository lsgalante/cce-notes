//! Single instance over the cce socket convention, in cce-browser's
//! claim-or-forward shape: the first `cce-notes` listens on
//! `/tmp/cce-notes-<WAYLAND_DISPLAY>.sock` (keyed by display, so shadow
//! sessions stay apart), and every later launch — and any app that wants a
//! note open — sends it one line and exits.
//!
//! Commands, one per line, each answered `ok`:
//!
//! - `open <path>[#heading]` — a vault path, an absolute path inside the
//!   vault, or a note name; everything after `open ` is the target, so a
//!   name may end in a number (`Chapter 3`). A 1-based line follows a tab:
//!   `open <path>\t<line>`. The older `open <path> <line>` still works:
//!   the app takes a trailing number as a line only when the whole text
//!   names no note but the text before it does (`NotesApp::open_target`).
//! - `daily [YYYY-MM-DD]` — open (creating it from the template) a day's note.
//! - `search <query>` — show the search pane holding `query` (`#tag` too).
//! - `show` — nothing but bringing the instance up.
//! - `current` — answered `ok <vault path>` (or a bare `ok` with no note
//!   open) straight from the listener thread; cce-graph's local graph
//!   polls it to follow the note on screen.
//! - `vault` — answered `ok <vault root>` (a bare `ok` without a vault),
//!   also from the listener: a launch with `--vault` asks it first, and
//!   runs as a window of its own rather than hand another vault's note to
//!   this one.
//!
//! The claim, the startup race it closes, the parked listener and the
//! bounded reads are `cce_ui::ipc::instance`'s; this module is the notes
//! protocol on top.

use std::sync::Mutex;

use crate::Message;

const PREFIX: &str = "cce-notes";
/// The open note's vault path, for `current`. Set by the app whenever it
/// changes; read by the listener without a trip through the event loop.
static CURRENT: Mutex<Option<String>> = Mutex::new(None);
/// The vault this instance shows, for `vault`.
static VAULT: Mutex<Option<String>> = Mutex::new(None);

pub fn set_current(path: Option<&str>) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = path.map(String::from);
}

pub fn set_vault(root: Option<&std::path::Path>) {
    *VAULT.lock().unwrap_or_else(|e| e.into_inner()) = root.map(|r| r.to_string_lossy().into_owned());
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Open { target: String, line: Option<usize> },
    Daily(Option<chrono::NaiveDate>),
    Search(String),
    Show,
}

impl Command {
    /// The command line's own arguments as a socket command:
    /// `cce-notes [open] <path> [line]`, `cce-notes daily [date]`,
    /// `cce-notes search <query…>`, `cce-notes show`, or nothing (also
    /// `show`). The verbs are reserved, as `daily` and `search` always were:
    /// a note named `show` opens with `cce-notes open show`.
    pub fn from_args(args: &[String]) -> Result<Command, String> {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            [] | ["show"] => Ok(Command::Show),
            ["daily"] => Ok(Command::Daily(None)),
            ["daily", d] => parse_date(d).map(|d| Command::Daily(Some(d))),
            ["search", rest @ ..] => Ok(Command::Search(rest.join(" "))),
            ["open", target] | [target] => Ok(Command::Open { target: absolute(target), line: None }),
            ["open", target, line] | [target, line] => {
                let line = line.parse::<usize>().map_err(|_| format!("not a line number: {line}"))?;
                Ok(Command::Open { target: absolute(target), line: Some(line) })
            }
            _ => Err("usage: cce-notes [open] <note> [line] | daily [YYYY-MM-DD] | search <query> | show".into()),
        }
    }

    pub fn to_line(&self) -> String {
        match self {
            Command::Open { target, line: Some(l) } => format!("open {target}\t{l}"),
            Command::Open { target, line: None } => format!("open {target}"),
            Command::Daily(Some(d)) => format!("daily {d}"),
            Command::Daily(None) => "daily".into(),
            Command::Search(q) => format!("search {q}"),
            Command::Show => "show".into(),
        }
    }

    pub fn parse(line: &str) -> Option<Command> {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        // `trim` would also eat the tab before a line number: only spaces.
        let rest = rest.trim_matches(' ');
        match verb {
            "open" if !rest.is_empty() => {
                // A line rides after a tab, which no note name holds. With
                // spaces only, the whole text is the target: "Chapter 3" is
                // a name, and the app sorts out an older sender's
                // `open Note 12` against the vault.
                match rest.rsplit_once('\t') {
                    Some((t, l)) if l.trim().parse::<usize>().is_ok() && !t.trim().is_empty() => {
                        Some(Command::Open { target: t.trim().to_string(), line: l.trim().parse().ok() })
                    }
                    _ => Some(Command::Open { target: rest.to_string(), line: None }),
                }
            }
            "daily" if rest.is_empty() => Some(Command::Daily(None)),
            "daily" => parse_date(rest).ok().map(|d| Command::Daily(Some(d))),
            "search" => Some(Command::Search(rest.to_string())),
            "show" => Some(Command::Show),
            _ => None,
        }
    }
}

fn parse_date(s: &str) -> Result<chrono::NaiveDate, String> {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| format!("not a date (YYYY-MM-DD): {s}"))
}

/// A path that exists here travels absolute: the instance's cwd differs.
/// Anything else (a note name, a vault path) goes as written.
fn absolute(target: &str) -> String {
    let (path, sub) = match target.split_once('#') {
        Some((p, s)) => (p, Some(s)),
        None => (target, None),
    };
    let p = std::path::Path::new(path);
    let abs = if p.exists() && !p.is_absolute() {
        std::fs::canonicalize(p).ok().and_then(|c| c.to_str().map(String::from))
    } else {
        None
    };
    match (abs, sub) {
        (Some(a), Some(s)) => format!("{a}#{s}"),
        (Some(a), None) => a,
        (None, _) => target.to_string(),
    }
}

/// Whether a launch for `wanted` (an explicit `--vault`) should go to the
/// running instance, given its answer to `vault`: yes when nothing answers
/// (the launch claims the socket as usual), when it shows that vault, or
/// when it predates the query; no when it shows another vault or none —
/// that launch runs as a window of its own instead.
pub fn running_instance_fits(reply: Option<&str>, wanted: &std::path::Path) -> bool {
    let Some(reply) = reply else { return true };
    let Some(rest) = reply.strip_prefix("ok") else { return true };
    let root = rest.trim();
    if root.is_empty() {
        return false;
    }
    let canon = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(std::path::Path::new(root)) == canon(wanted)
}

/// Ask the running instance, if any, which vault it shows.
pub fn running_vault_reply() -> Option<String> {
    cce_ui::ipc::instance::forward(PREFIX, "vault")
}

/// Hand `cmd` to a running instance, or claim the socket. True when a
/// running instance took it and this process should exit.
pub fn forward_or_claim(cmd: &Command) -> bool {
    cce_ui::ipc::instance::forward_or_claim(PREFIX, &cmd.to_line())
}

/// Serve the claimed listener on a thread, feeding the app's loop. False
/// when this process holds no listener (single-instance handling failed
/// and it runs standalone).
pub fn spawn_listener(sender: calloop::channel::Sender<Message>) -> bool {
    cce_ui::ipc::instance::serve(move |line| {
        // Answered from here, without a trip through the event loop.
        if let Some(answer) = query(line) {
            return Some(answer);
        }
        match Command::parse(line) {
            Some(cmd) => {
                sender.send(Message::Command(cmd)).ok()?;
                Some("ok".into())
            }
            None => Some("error unknown command".into()),
        }
    })
}

/// The queries the listener answers from shared state: `current`, `vault`.
fn query(line: &str) -> Option<String> {
    let slot = match line {
        "current" => &CURRENT,
        "vault" => &VAULT,
        _ => return None,
    };
    Some(match slot.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        Some(v) => format!("ok {v}"),
        None => "ok".to_string(),
    })
}

pub fn cleanup() {
    cce_ui::ipc::instance::cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn args_and_lines_round_trip() {
        let cases = [
            (vec![], Command::Show),
            (vec!["show"], Command::Show),
            (vec!["open", "show"], Command::Open { target: "show".into(), line: None }),
            (vec!["Note"], Command::Open { target: "Note".into(), line: None }),
            (vec!["open", "My Note#Sec", "12"], Command::Open { target: "My Note#Sec".into(), line: Some(12) }),
            (vec!["daily"], Command::Daily(None)),
            (vec!["daily", "2026-10-01"], Command::Daily(chrono::NaiveDate::from_ymd_opt(2026, 10, 1))),
            (vec!["search", "two", "words"], Command::Search("two words".into())),
        ];
        for (a, want) in cases {
            let got = Command::from_args(&args(&a)).unwrap();
            assert_eq!(got, want, "{a:?}");
            assert_eq!(Command::parse(&got.to_line()), Some(want));
        }
        assert!(Command::from_args(&args(&["daily", "tomorrow"])).is_err());
        assert!(Command::parse("open ").is_none());
        assert!(Command::parse("bogus").is_none());
        // A note whose name is a number is a target, not a line.
        assert_eq!(Command::parse("open 2026"), Some(Command::Open { target: "2026".into(), line: None }));
    }

    #[test]
    fn a_name_ending_in_a_number_stays_whole() {
        let open = |t: &str, l| Some(Command::Open { target: t.into(), line: l });
        // `cce-notes "Chapter 3"` travels and arrives as that name.
        let cmd = Command::from_args(&args(&["Chapter 3"])).unwrap();
        assert_eq!(cmd.to_line(), "open Chapter 3");
        assert_eq!(Command::parse(&cmd.to_line()), open("Chapter 3", None));
        // A line rides after a tab, even after such a name.
        let cmd = Command::from_args(&args(&["open", "Chapter 3", "12"])).unwrap();
        assert_eq!(Command::parse(&cmd.to_line()), open("Chapter 3", Some(12)));
        // The older space form arrives whole; the app resolves it.
        assert_eq!(Command::parse("open Note 12"), open("Note 12", None));
        assert_eq!(Command::parse("open \t5"), open("\t5", None));
    }

    #[test]
    fn a_launch_for_another_vault_runs_on_its_own() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let reply = |p: &std::path::Path| format!("ok {}", p.display());
        assert!(running_instance_fits(None, a.path()), "nothing running: claim as usual");
        assert!(running_instance_fits(Some(&reply(a.path())), a.path()));
        assert!(!running_instance_fits(Some(&reply(b.path())), a.path()));
        assert!(!running_instance_fits(Some("ok"), a.path()), "an instance without a vault");
        assert!(running_instance_fits(Some("error unknown command"), a.path()), "an older instance");
        // Spelled differently, the same folder.
        let dotted = a.path().join(".");
        assert!(running_instance_fits(Some(&reply(&dotted)), a.path()));
    }

    #[test]
    fn the_listener_answers_current_and_vault() {
        set_vault(Some(std::path::Path::new("/v")));
        set_current(Some("a/N.md"));
        assert_eq!(query("vault").as_deref(), Some("ok /v"));
        assert_eq!(query("current").as_deref(), Some("ok a/N.md"));
        assert_eq!(query("open x"), None);
        set_vault(None);
        assert_eq!(query("vault").as_deref(), Some("ok"));
    }
}
