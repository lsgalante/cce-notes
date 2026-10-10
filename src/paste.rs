//! Pasting and dropping images, as Obsidian does: a picture on
//! the clipboard — a screenshot, an image copied in a browser — or image
//! files copied in a file manager become files in the vault's attachment
//! folder (`cce_vault::attachments`, Obsidian's own setting) and `![[…]]`
//! embeds at the caret. Anything else pastes as text, through the editor.
//!
//! The clipboard is read with `wl-paste`, as cce-ui's text paste reads it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a paste waits on the clipboard. Ctrl+V decides between an
/// image and a text paste on the UI thread, and the clipboard's owner is
/// another app: one that never answers froze the editor (`output()` waits
/// forever). Past this, it pastes as text.
const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

/// Image types, best first, and the extension each is saved with.
const IMAGE_TYPES: [(&str, &str); 5] =
    [("image/png", "png"), ("image/jpeg", "jpg"), ("image/webp", "webp"), ("image/gif", "gif"), ("image/bmp", "bmp")];

#[derive(Debug, PartialEq)]
pub enum Clip {
    /// Picture bytes, and the extension they are saved with.
    Image { bytes: Vec<u8>, ext: &'static str },
    /// Image files copied in a file manager.
    Files(Vec<PathBuf>),
}

/// What a drop takes, best first: a browser's dragged picture as pixels,
/// else the file list a file manager's drag carries.
pub const DROP_MIMES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp", "text/uri-list"];

/// What a drop of `data` as `mime` attaches, if anything.
pub fn from_drop(mime: &str, data: &[u8]) -> Option<Clip> {
    if let Some((_, ext)) = IMAGE_TYPES.iter().find(|(m, _)| *m == mime) {
        return (!data.is_empty()).then(|| Clip::Image { bytes: data.to_vec(), ext });
    }
    if mime == "text/uri-list" {
        let files = image_files(&String::from_utf8_lossy(data));
        return (!files.is_empty()).then_some(Clip::Files(files));
    }
    None
}

/// What on the clipboard a paste should turn into attachments, if anything.
pub fn read() -> Option<Clip> {
    let types = wl_paste(&["--list-types"])?;
    let types: Vec<&str> = std::str::from_utf8(&types).ok()?.lines().map(str::trim).collect();
    log::debug!("paste: clipboard offers {types:?}");
    // Copied files first: a file manager offers their paths as text/plain
    // too. A list naming no local image (a browser's copied link) falls
    // through to the text paste.
    if types.contains(&"text/uri-list") {
        let list = wl_paste(&["--type", "text/uri-list"])?;
        let files = image_files(&String::from_utf8_lossy(&list));
        if !files.is_empty() {
            return Some(Clip::Files(files));
        }
    }
    let (mime, ext) = image_offer(&types)?;
    let bytes = wl_paste(&["--type", mime])?;
    (!bytes.is_empty()).then_some(Clip::Image { bytes, ext })
}

/// The picture type to take, if any. Only when no plain text is offered
/// too: copying text from a browser or an office app often carries an
/// image rendering of it as well, and that paste must stay text. A copied
/// image (browser, screenshot tool) offers no text/plain.
fn image_offer(types: &[&str]) -> Option<(&'static str, &'static str)> {
    if types.iter().any(|t| t.starts_with("text/plain")) {
        return None;
    }
    IMAGE_TYPES.iter().copied().find(|(m, _)| types.contains(m))
}

/// The local image files a `text/uri-list` names (comments and other files
/// skipped).
fn image_files(list: &str) -> Vec<PathBuf> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        // `file://host/path`: only the local form, with an empty host.
        .filter(|p| p.starts_with('/'))
        .map(|p| PathBuf::from(percent_encoding::percent_decode_str(p).decode_utf8_lossy().into_owned()))
        .filter(|p| cce_vault::markdown::is_image(&p.to_string_lossy()) && p.is_file())
        .collect()
}

fn wl_paste(args: &[&str]) -> Option<Vec<u8>> {
    let mut cmd = Command::new("wl-paste");
    cmd.args(args);
    let out = output_within(cmd, PASTE_TIMEOUT);
    if out.is_none() {
        log::debug!("paste: wl-paste {args:?} failed or timed out");
    }
    out
}

/// Run `cmd` and collect its stdout, or `None` when it fails or takes past
/// `limit` (then it is killed). The output is read on its own thread: a
/// picture larger than the pipe would otherwise hold the child mid-write
/// until the deadline, and look like a hang.
fn output_within(mut cmd: Command, limit: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + limit;
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let Ok(bytes) = rx.recv_timeout(limit) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    exit_by(&mut child, deadline)?.success().then_some(bytes)
}

/// The child's exit status once it exits, or `None` (killing it) when it
/// has not by `deadline`.
fn exit_by(child: &mut std::process::Child, deadline: Instant) -> Option<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait().ok()? {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Write `clip` into `dir` (made if missing): a picture as `Pasted image
/// <timestamp>.<ext>`, Obsidian's name; files under their own names. Names
/// that clash are numbered. The paths written.
pub fn store(clip: Clip, dir: &Path, now: chrono::NaiveDateTime) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir)?;
    match clip {
        Clip::Image { bytes, ext } => {
            let name = format!("Pasted image {}.{ext}", now.format("%Y%m%d%H%M%S"));
            let path = cce_vault::attachments::unique_path(dir, &name);
            std::fs::write(&path, bytes)?;
            Ok(vec![path])
        }
        Clip::Files(files) => {
            let mut out = Vec::new();
            for f in files {
                let Some(name) = f.file_name() else { continue };
                let path = cce_vault::attachments::unique_path(dir, &name.to_string_lossy());
                std::fs::copy(&f, &path)?;
                out.push(path);
            }
            Ok(out)
        }
    }
}

/// The text a paste of `embeds` (`![[…]]` lines) inserts between `before`
/// and `after` (the caret line's text on either side of the selection):
/// each embed on its own line, since only a line that is one embed shows
/// as the picture. Returns (text, its length up to where the caret goes).
pub fn insertion(embeds: &[String], before: &str, after: &str) -> (String, usize) {
    let mut text = String::new();
    if !before.trim().is_empty() {
        text.push('\n');
    }
    text.push_str(&embeds.join("\n"));
    let caret = text.len();
    if !after.trim().is_empty() {
        text.push('\n');
    }
    (text, caret)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clipboard_that_never_answers_times_out() {
        let sh = |script: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", script]);
            c
        };
        let started = Instant::now();
        assert_eq!(output_within(sh("sleep 10"), Duration::from_millis(200)), None);
        assert!(started.elapsed() < Duration::from_secs(2), "it waited for the child");
        assert_eq!(output_within(sh("printf hi"), PASTE_TIMEOUT).as_deref(), Some(&b"hi"[..]));
        assert_eq!(output_within(sh("echo x; exit 1"), PASTE_TIMEOUT), None);
        // Larger than a pipe holds: read as it comes, not mistaken for a hang.
        let big = output_within(sh("head -c 1000000 /dev/zero"), PASTE_TIMEOUT).unwrap();
        assert_eq!(big.len(), 1_000_000);
    }

    #[test]
    fn text_wins_over_an_image_rendering() {
        assert_eq!(image_offer(&["image/png", "text/html"]), Some(("image/png", "png")));
        assert_eq!(image_offer(&["image/jpeg", "image/png"]), Some(("image/png", "png")));
        assert_eq!(image_offer(&["text/html", "text/plain;charset=utf-8", "image/png"]), None);
        assert_eq!(image_offer(&["text/uri-list"]), None);
        assert_eq!(image_offer(&[]), None);
    }

    #[test]
    fn uri_lists_keep_local_images() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("my pic.png");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(dir.path().join("doc.pdf"), b"x").unwrap();
        let list = format!(
            "# copied\nfile://{}\nfile://{}/doc.pdf\nfile://{}/gone.png\nhttps://x.y/z.png\n",
            a.display().to_string().replace(' ', "%20"),
            dir.path().display(),
            dir.path().display()
        );
        assert_eq!(image_files(&list), vec![a]);
    }

    #[test]
    fn drops_take_pixels_or_image_files() {
        assert_eq!(from_drop("image/jpeg", b"jpg"), Some(Clip::Image { bytes: b"jpg".to_vec(), ext: "jpg" }));
        assert_eq!(from_drop("image/png", b""), None);
        assert_eq!(from_drop("text/uri-list", b"https://x.y/a.png\r\n"), None);
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.webp");
        std::fs::write(&a, b"x").unwrap();
        let list = format!("file://{}\r\n", a.display());
        assert_eq!(from_drop("text/uri-list", list.as_bytes()), Some(Clip::Files(vec![a])));
        assert_eq!(from_drop("text/html", b"<img>"), None);
    }

    #[test]
    fn stores_with_obsidian_names() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap().and_hms_opt(21, 35, 12).unwrap();
        let p = store(Clip::Image { bytes: b"png".to_vec(), ext: "png" }, &dir.path().join("att"), now).unwrap();
        assert_eq!(p, vec![dir.path().join("att/Pasted image 20261001213512.png")]);
        let p = store(Clip::Image { bytes: b"png".to_vec(), ext: "png" }, &dir.path().join("att"), now).unwrap();
        assert_eq!(p, vec![dir.path().join("att/Pasted image 20261001213512 1.png")]);
    }

    #[test]
    fn embeds_go_on_their_own_lines() {
        let e = vec!["![[a.png]]".to_string()];
        assert_eq!(insertion(&e, "", ""), ("![[a.png]]".into(), 10));
        assert_eq!(insertion(&e, "text ", " more"), ("\n![[a.png]]\n".into(), 11));
        let two = vec!["![[a.png]]".to_string(), "![[b.png]]".to_string()];
        assert_eq!(insertion(&two, "", "").0, "![[a.png]]\n![[b.png]]");
    }
}
