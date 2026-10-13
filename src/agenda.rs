//! The day's agenda for a daily note's ```` ```agenda ```` block, from the
//! cce-sync daemon (`agenda YYYY-MM-DD` on its socket, which asks the
//! server's Google bridge). This app holds no token: the daemon does. The
//! ask runs on a thread and comes back as `Message::Agenda`; the block is
//! filled in the reading view only (`cce_vault::agenda::fill`), never in
//! the file.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Ask the daemon again for a day after this long.
pub const AGAIN: Duration = Duration::from_secs(5 * 60);

fn socket() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("cce/cce-sync.sock")
}

/// The agenda's lines for `day` (`YYYY-MM-DD`), or one line saying why
/// there are none. Blocks for up to 15 s: call it off the UI thread.
pub fn fetch(day: &str) -> Vec<String> {
    let ask = || -> std::io::Result<String> {
        let mut s = UnixStream::connect(socket())?;
        s.set_read_timeout(Some(Duration::from_secs(15)))?;
        s.write_all(format!("agenda {day}\n").as_bytes())?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line)?;
        Ok(line)
    };
    match ask() {
        Err(_) => vec!["(the agenda comes through the cce-sync daemon, which is not running)".into()],
        Ok(line) => answer(&line),
    }
}

/// The daemon's answer as lines.
pub fn answer(line: &str) -> Vec<String> {
    let line = line.trim();
    if let Some(e) = line.strip_prefix("error ") {
        return vec![format!("(the agenda could not be loaded: {e})")];
    }
    match serde_json::from_str::<cce_sync::agenda::Agenda>(line) {
        Ok(a) => a.lines(),
        Err(_) => vec!["(the agenda could not be read)".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers() {
        let ok = r#"{"date":"2026-10-13","connected":true,"time_zone":"Europe/Berlin","events":[{"id":"p/1","title":"Standup","start":"09:00","end":"09:15","all_day":false}]}"#;
        assert_eq!(answer(ok), vec!["09:00–09:15  Standup"]);
        assert_eq!(answer("error offline"), vec!["(the agenda could not be loaded: offline)"]);
        assert_eq!(answer("{}"), vec!["(the agenda could not be read)"]);
    }
}
