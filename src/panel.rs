//! A scrollable list of rows for the side panes: section headers, note
//! titles and indented line snippets, each optionally clickable. The
//! backlinks and outline pane on the right and the search pane on the left
//! all draw through this, so they hover, scroll and click alike.

use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{PaintCtx, TextAttrs};
use cce_ui::widget::{Bounds, MouseScrollDelta, ScrollMotion};

use crate::reading::srgb_u8;

pub const ROW_H: f32 = 22.0;
const PAD: f32 = 6.0;
const INDENT: f32 = 14.0;

/// What a click on a row does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Open a note, optionally at a 0-based source line.
    Open { path: String, line: Option<usize> },
    /// Bring a 0-based line of the open note into view.
    Line(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A section label ("Linked mentions (3)").
    Header,
    /// A note's name.
    Title,
    /// A line of text under a title, or an outline heading.
    Line,
    /// An explanatory line ("No backlinks").
    Note,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub depth: usize,
    pub text: String,
    pub kind: Kind,
    pub action: Option<Action>,
}

impl Item {
    pub fn header(text: impl Into<String>) -> Item {
        Item { depth: 0, text: text.into(), kind: Kind::Header, action: None }
    }
    pub fn note(text: impl Into<String>) -> Item {
        Item { depth: 0, text: text.into(), kind: Kind::Note, action: None }
    }
    pub fn title(text: impl Into<String>, action: Action) -> Item {
        Item { depth: 0, text: text.into(), kind: Kind::Title, action: Some(action) }
    }
    pub fn line(depth: usize, text: impl Into<String>, action: Action) -> Item {
        Item { depth, text: text.into(), kind: Kind::Line, action: Some(action) }
    }
}

#[derive(Default)]
pub struct Panel {
    pub items: Vec<Item>,
    pub scroll: f32,
    motion: ScrollMotion,
    pub hover: Option<usize>,
}

impl Panel {
    /// Replace the rows. The scroll stays when the rows are an update of
    /// the same content (a refresh), and resets when `reset` says the
    /// content is for something else (another note, another query).
    pub fn set(&mut self, items: Vec<Item>, reset: bool) {
        self.items = items;
        self.hover = None;
        if reset {
            self.scroll = 0.0;
            self.motion = ScrollMotion::new();
        }
    }

    fn max_scroll(&self, area: Rect) -> f32 {
        (self.items.len() as f32 * ROW_H + 2.0 * PAD - area.height).max(0.0)
    }

    pub fn item_at(&self, area: Rect, x: f32, y: f32) -> Option<usize> {
        if x < area.x || x > area.x + area.width || y < area.y || y > area.y + area.height {
            return None;
        }
        let i = ((y - area.y - PAD + self.scroll) / ROW_H).floor();
        if i < 0.0 {
            return None;
        }
        let i = i as usize;
        (i < self.items.len() && self.items[i].action.is_some()).then_some(i)
    }

    /// Update the hovered row; true when it changed.
    pub fn hover_at(&mut self, area: Rect, x: f32, y: f32) -> bool {
        let h = self.item_at(area, x, y);
        let changed = h != self.hover;
        self.hover = h;
        changed
    }

    pub fn click(&self, area: Rect, x: f32, y: f32) -> Option<Action> {
        self.item_at(area, x, y).and_then(|i| self.items[i].action.clone())
    }

    pub fn wheel(&mut self, delta: &MouseScrollDelta, area: Rect) -> bool {
        let max = self.max_scroll(area);
        self.motion.reconcile(0.0, self.scroll);
        let moved = self.motion.apply(delta, (ROW_H, ROW_H), Bounds::max(0.0), Bounds::max(max));
        self.scroll = self.motion.y.pos();
        moved
    }

    /// Advance a wheel glide; true while it moves.
    pub fn tick(&mut self, dt: f32, area: Rect) -> bool {
        if !self.motion.is_animating() {
            return false;
        }
        let max = self.max_scroll(area);
        self.motion.tick(dt, Bounds::max(0.0), Bounds::max(max));
        self.scroll = self.motion.y.pos();
        true
    }

    pub fn clamp(&mut self, area: Rect) {
        let max = self.max_scroll(area);
        if self.scroll > max {
            self.scroll = max;
            self.motion.y.jump_to(max);
        }
    }

    pub fn paint(&self, pc: &mut PaintCtx, area: Rect) {
        let (family, size) = cce_ui::layout::tree_font_parsed();
        let small = (size * 0.9).round();
        let body = "sans-serif".to_string();
        pc.clip(area, |pc| {
            for (i, item) in self.items.iter().enumerate() {
                let y = area.y + PAD + i as f32 * ROW_H - self.scroll;
                if y + ROW_H < area.y || y > area.y + area.height {
                    continue;
                }
                let r = Rect { x: area.x + 4.0, y, width: area.width - 8.0, height: ROW_H };
                if self.hover == Some(i) {
                    pc.rounded_rect(r, 4.0, (true, true, true, true), [1.0, 1.0, 1.0, 0.05]);
                }
                let x = r.x + 6.0 + item.depth as f32 * INDENT;
                let bounds = Some([x, r.y.max(area.y), r.x + r.width - 4.0, (r.y + r.height).min(area.y + area.height)]);
                let (font, fsize, color, attrs) = match item.kind {
                    Kind::Header => (family.clone(), small, cce_ui::colors::TEXT_DIM, TextAttrs::default()),
                    Kind::Title => (family.clone(), size, cce_ui::colors::TEXT_FG, TextAttrs::default()),
                    Kind::Line => (body.clone(), small, cce_ui::colors::TEXT_FG, TextAttrs::default()),
                    Kind::Note => (body.clone(), small, cce_ui::colors::TEXT_DIM, TextAttrs { italic: true, weight: None }),
                };
                let ty = cce_ui::layout::align_text_y(r.y, r.height, fsize, 0.0);
                pc.text_attrs(item.text.clone(), x, ty, fsize, srgb_u8(color), Some(font), bounds, attrs);
            }
        });
    }
}
