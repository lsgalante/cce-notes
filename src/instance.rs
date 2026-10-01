//! Single instance over the cce socket convention, in cce-browser's
//! claim-or-forward shape: the first `cce-notes` listens on
//! `/tmp/cce-notes-<WAYLAND_DISPLAY>.sock` (keyed by display, so shadow
//! sessions stay apart), and every later launch — and any app that wants a
//! note open — sends it one line and exits.
//!
//! Commands, one per line, each answered `ok`:
//!
//! - `open <path>[#heading] [line]` — a vault path, an absolute path inside
//!   the vault, or a note name; `line` is 1-based.
//! - `daily [YYYY-MM-DD]` — open (creating it from the template) a day's note.
//! - `search <query>` — show the search pane holding `query` (`#tag` too).
//! - `show` — nothing but bringing the instance up.
//! - `current` — answered `ok <vault path>` (or a bare `ok` with no note
//!   open) straight from the listener thread; cce-graph's local graph
//!   polls it to follow the note on screen.
//!
//! Connect before binding, as cce-browser does: a refused connect means a
//! crashed instance left its socket file, which is removed; losing the bind
//! to a simultaneous launch falls back to one more connect.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;

use crate::Message;

const PREFIX: &str = "cce-notes";

static CLAIMED: Mutex<Option<UnixListener>> = Mutex::new(None);
static OWNED_PATH: Mutex<Option<String>> = Mutex::new(None);
/// The open note's vault path, for `current`. Set by the app whenever it
/// changes; read by the listener without a trip through the event loop.
static CURRENT: Mutex<Option<String>> = Mutex::new(None);

pub fn set_current(path: Option<&str>) {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = path.map(String::from);
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
    /// `cce-notes search <query…>`, or nothing.
    pub fn from_args(args: &[String]) -> Result<Command, String> {
        let words: Vec<&str> = args.iter().map(String::as_str).collect();
        match words.as_slice() {
            [] => Ok(Command::Show),
            ["daily"] => Ok(Command::Daily(None)),
            ["daily", d] => parse_date(d).map(|d| Command::Daily(Some(d))),
            ["search", rest @ ..] => Ok(Command::Search(rest.join(" "))),
            ["open", target] | [target] => Ok(Command::Open { target: absolute(target), line: None }),
            ["open", target, line] | [target, line] => {
                let line = line.parse::<usize>().map_err(|_| format!("not a line number: {line}"))?;
                Ok(Command::Open { target: absolute(target), line: Some(line) })
            }
            _ => Err("usage: cce-notes [open] <note> [line] | daily [YYYY-MM-DD] | search <query>".into()),
        }
    }

    pub fn to_line(&self) -> String {
        match self {
            Command::Open { target, line: Some(l) } => format!("open {target} {l}"),
            Command::Open { target, line: None } => format!("open {target}"),
            Command::Daily(Some(d)) => format!("daily {d}"),
            Command::Daily(None) => "daily".into(),
            Command::Search(q) => format!("search {q}"),
            Command::Show => "show".into(),
        }
    }

    pub fn parse(line: &str) -> Option<Command> {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        match verb {
            "open" if !rest.is_empty() => {
                // A trailing number is a line, unless it is all there is.
                match rest.rsplit_once(' ') {
                    Some((t, l)) if l.parse::<usize>().is_ok() => {
                        Some(Command::Open { target: t.trim().to_string(), line: l.parse().ok() })
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

/// Hand `cmd` to a running instance, or claim the socket. True when a
/// running instance took it and this process should exit.
pub fn forward_or_claim(cmd: &Command) -> bool {
    let path = cce_ui::ipc::socket_path(PREFIX);
    if try_forward(&path, cmd) {
        return true;
    }
    if std::path::Path::new(&path).exists() {
        let _ = std::fs::remove_file(&path);
    }
    match UnixListener::bind(&path) {
        Ok(listener) => {
            *CLAIMED.lock().unwrap() = Some(listener);
            *OWNED_PATH.lock().unwrap() = Some(path);
            false
        }
        Err(_) => try_forward(&path, cmd),
    }
}

fn try_forward(path: &str, cmd: &Command) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    if stream.write_all(format!("{}\n", cmd.to_line()).as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok()
}

/// Serve the claimed listener on a thread, feeding the app's loop. False
/// when this process holds no listener (single-instance handling failed
/// and it runs standalone).
pub fn spawn_listener(sender: calloop::channel::Sender<Message>) -> bool {
    let Some(listener) = CLAIMED.lock().unwrap().take() else {
        return false;
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let mut reader = BufReader::new(conn);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            if line.trim() == "current" {
                let cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let reply = match cur {
                    Some(p) => format!("ok {p}\n"),
                    None => "ok\n".to_string(),
                };
                let _ = reader.get_mut().write_all(reply.as_bytes());
                continue;
            }
            let reply: &[u8] = match Command::parse(line.trim()) {
                Some(cmd) => {
                    if sender.send(Message::Command(cmd)).is_err() {
                        return;
                    }
                    b"ok\n"
                }
                None => b"error unknown command\n",
            };
            let _ = reader.get_mut().write_all(reply);
        }
    });
    true
}

pub fn cleanup() {
    if let Some(path) = OWNED_PATH.lock().unwrap().take() {
        let _ = std::fs::remove_file(path);
    }
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
}
