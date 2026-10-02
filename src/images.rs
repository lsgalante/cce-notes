//! Embedded images (`![[pic.png]]`, `![](pic.png)`) for the reading view
//! and the live-preview editor.
//!
//! Both ask by an embed's **link text** while they lay out and paint
//! ([`Images::lookup`], shared with the editor through an `Rc`). A link not
//! seen yet is only recorded; after the frame, [`Images::pump`] resolves it
//! the way every vault link resolves (`Index::resolve_text`, relative to the
//! open note), and an image file is decoded on a thread. The decode comes
//! back as [`crate::Message::ImageDecoded`] — which also wakes an idle loop —
//! and [`Images::decoded`] uploads it; the host then lays out again so the
//! link becomes the picture. A decode that fails leaves the link showing.
//!
//! Ids belong to one renderer: [`Images::renderer_reset`] forgets every one
//! after a reconnect and the next frame asks again. Decoded images are kept
//! for the last [`KEEP`] used, so walking back and forth between notes does
//! not decode the same picture twice.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use cce_ui::widget::EmbedImage;
use cce_vault::Index;

/// Decoded images kept across notes.
const KEEP: usize = 32;
/// The largest texture side: a column is under 800 logical px, 1600 at
/// output scale 2, so more is memory nobody sees.
const MAX_TEX: u32 = 2048;

/// A decoded image, straight RGBA8, and the size it shows at before any
/// fitting — its own pixels for a raster (even when the texture was made
/// smaller), its intrinsic size for an SVG.
#[derive(Clone)]
pub struct Decoded {
    pub pixels: Vec<u8>,
    pub tex: (u32, u32),
    pub natural: (u32, u32),
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Decoded {{ tex: {:?}, natural: {:?} }}", self.tex, self.natural)
    }
}

enum Entry {
    Loading,
    Failed,
    Ready { image: EmbedImage, used: u64 },
}

#[derive(Default)]
struct Inner {
    /// Link text → vault path, for the open note (`None`: not an image, or
    /// it resolves nowhere). Cleared when the note or the vault changes.
    links: HashMap<String, Option<String>>,
    /// Link texts asked for that `links` does not know yet.
    asked: HashSet<String>,
    entries: HashMap<String, Entry>,
    clock: u64,
}

#[derive(Clone, Default)]
pub struct Images(Rc<RefCell<Inner>>);

impl Images {
    /// The uploaded image an embed's link text shows, if it is ready.
    pub fn lookup(&self, link: &str) -> Option<EmbedImage> {
        let mut inner = self.0.borrow_mut();
        let path = match inner.links.get(link) {
            Some(Some(p)) => p.clone(),
            Some(None) => return None,
            None => {
                inner.asked.insert(link.to_string());
                return None;
            }
        };
        inner.clock += 1;
        let now = inner.clock;
        match inner.entries.get_mut(&path) {
            Some(Entry::Ready { image, used }) => {
                *used = now;
                Some(*image)
            }
            _ => None,
        }
    }

    /// Resolve what was asked for since the last pump and start decoding
    /// the images among it. `done` carries a decode back to the app. True
    /// when a link resolved to an image already decoded: it showed as a
    /// link this frame, and the host must lay out again to show it.
    pub fn pump(&self, index: &Index, current: Option<&str>, done: impl Fn(String, Option<Decoded>) + Clone + Send + 'static) -> bool {
        let mut inner = self.0.borrow_mut();
        let asked: Vec<String> = inner.asked.drain().collect();
        let mut ready = false;
        for link in asked {
            let path = index.resolve_text(current, &link).filter(|p| cce_vault::markdown::is_image(p));
            inner.links.insert(link, path.clone());
            let Some(path) = path else { continue };
            if let Some(e) = inner.entries.get(&path) {
                ready |= matches!(e, Entry::Ready { .. });
                continue;
            }
            inner.entries.insert(path.clone(), Entry::Loading);
            let abs = index.abs(&path);
            let done = done.clone();
            std::thread::spawn(move || {
                let decoded = decode(&abs);
                if decoded.is_none() {
                    log::warn!("could not decode {}", abs.display());
                }
                done(path, decoded);
            });
        }
        ready
    }

    /// A decode finished: upload it. True when the image is new to the
    /// screen (the host lays out again).
    pub fn decoded(&self, path: String, decoded: Option<Decoded>) -> bool {
        let mut inner = self.0.borrow_mut();
        // Dropped (vault change, renderer reset) while it decoded.
        if !matches!(inner.entries.get(&path), Some(Entry::Loading)) {
            return false;
        }
        let Some(d) = decoded else {
            inner.entries.insert(path, Entry::Failed);
            return false;
        };
        let id = cce_ui::vk::upload_rgba_mipmapped(d.pixels, d.tex.0, d.tex.1);
        inner.clock += 1;
        let used = inner.clock;
        let image = EmbedImage { id, width: d.natural.0, height: d.natural.1 };
        inner.entries.insert(path, Entry::Ready { image, used });
        evict(&mut inner);
        true
    }

    /// The open note changed: its links resolve afresh.
    pub fn note_changed(&self) {
        self.0.borrow_mut().links.clear();
    }

    /// Files changed in the vault: links resolve afresh (an image may have
    /// appeared), and a changed image decodes again. True when an image
    /// on screen went away.
    pub fn vault_changed(&self, index: &Index, paths: &[PathBuf]) -> bool {
        let mut inner = self.0.borrow_mut();
        inner.links.clear();
        let mut dropped = false;
        for p in paths {
            let Some(rel) = index.rel(p) else { continue };
            if let Some(Entry::Ready { image, .. }) = inner.entries.remove(&rel) {
                cce_ui::vk::free_image(image.id);
                dropped = true;
            }
        }
        dropped
    }

    /// A new renderer: every id is dead (nothing to free). Forget them all.
    pub fn renderer_reset(&self) {
        let mut inner = self.0.borrow_mut();
        inner.entries.clear();
        inner.links.clear();
    }
}

/// Free the least recently drawn images past [`KEEP`].
fn evict(inner: &mut Inner) {
    let mut ready: Vec<(u64, String)> = inner
        .entries
        .iter()
        .filter_map(|(p, e)| match e {
            Entry::Ready { used, .. } => Some((*used, p.clone())),
            _ => None,
        })
        .collect();
    if ready.len() <= KEEP {
        return;
    }
    ready.sort();
    for (_, path) in &ready[..ready.len() - KEEP] {
        if let Some(Entry::Ready { image, .. }) = inner.entries.remove(path) {
            cce_ui::vk::free_image(image.id);
        }
    }
}

/// Read and decode an image file (off the UI thread).
pub fn decode(path: &Path) -> Option<Decoded> {
    let bytes = std::fs::read(path).ok()?;
    let is_svg = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    if is_svg {
        return decode_svg(&bytes);
    }
    let img = image::load_from_memory(&bytes).ok()?;
    let natural = (img.width(), img.height());
    let img = if natural.0.max(natural.1) > MAX_TEX {
        img.resize(MAX_TEX, MAX_TEX, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    let tex = rgba.dimensions();
    Some(Decoded { pixels: rgba.into_raw(), tex, natural })
}

/// An SVG shows at its own size; it is rasterised at twice that (sharp at
/// output scale 2), within [`MAX_TEX`].
fn decode_svg(bytes: &[u8]) -> Option<Decoded> {
    let tree = resvg::usvg::Tree::from_data(bytes, &resvg::usvg::Options::default(), cce_ui::widget::get_font_db()).ok()?;
    let size = tree.size();
    let natural = (size.width().round().max(1.0) as u32, size.height().round().max(1.0) as u32);
    let px = (natural.0.max(natural.1) * 2).min(MAX_TEX);
    let (pixels, w, h) = cce_ui::rasterize_svg(bytes, px)?;
    Some(Decoded { pixels, tex: (w, h), natural })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_shrinks_big_rasters() {
        let dir = tempfile::tempdir().unwrap();
        let small = dir.path().join("s.png");
        image::RgbaImage::new(30, 20).save(&small).unwrap();
        let d = decode(&small).unwrap();
        assert_eq!((d.tex, d.natural), ((30, 20), (30, 20)));
        assert_eq!(d.pixels.len(), 30 * 20 * 4);
        let big = dir.path().join("b.png");
        image::RgbaImage::new(4096, 1024).save(&big).unwrap();
        let d = decode(&big).unwrap();
        assert_eq!((d.tex, d.natural), ((2048, 512), (4096, 1024)));
        assert!(decode(&dir.path().join("missing.png")).is_none());
    }

    #[test]
    fn decodes_svg_at_its_own_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.svg");
        std::fs::write(&p, r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20"><rect width="40" height="20" fill="red"/></svg>"#).unwrap();
        let d = decode(&p).unwrap();
        assert_eq!((d.natural, d.tex), ((40, 20), (80, 40)));
    }

    #[test]
    fn lookups_are_recorded_until_pumped() {
        let images = Images::default();
        assert_eq!(images.lookup("a.png"), None);
        assert!(images.0.borrow().asked.contains("a.png"));
    }
}
