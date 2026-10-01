//! The reading view: a note's blocks (`cce_vault::markdown`) laid out at a
//! width into draw items and click targets.
//!
//! cce-ui's text prim draws one run in one style, so a paragraph mixing
//! bold, links and code is laid out here word by word: each word is
//! measured through the same shaping entry the renderer draws with
//! ([`Measure`]), wrapped greedily, and consecutive words of one style on
//! one line merge back into a single prim. Layout runs once per note and
//! width; painting only translates by the scroll and culls.
//!
//! This is the app-local first cut of the `MarkdownView` the proposal puts
//! in cce-ui. It moves there once a second surface (note cards, graph
//! previews) needs it — and cce-ui is free to take it.

use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{Cap, PaintCtx, TextAttrs};
use cce_vault::markdown::{Block, Callout, ListItem, Span, SpanLink, Style};

/// Text widths in logical px, for one run in one style.
pub trait Measure {
    fn width(&mut self, text: &str, size: f32, font: &str, attrs: TextAttrs) -> f32;
}

/// Fonts and the body size the layout works from.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub body_font: String,
    pub mono_font: String,
    pub size: f32,
}

impl Theme {
    fn line_h(&self, size: f32) -> f32 {
        (size * 1.5).ceil()
    }
}

// Colours. TODO(style): the toolkit has no link or highlight colour yet;
// these follow Obsidian's dark theme until it does. FG, DIM and the white
// washes are already linear; the rest are written in sRGB (as a theme
// states them) and go through `lin` where they are used.
const FG: [f32; 4] = cce_ui::colors::TEXT_FG;
const DIM: [f32; 4] = cce_ui::colors::TEXT_DIM;
const LINK: [f32; 4] = [0.66, 0.55, 0.98, 1.0];
const LINK_UNRESOLVED: [f32; 4] = [0.50, 0.44, 0.70, 1.0];
const TAG: [f32; 4] = [0.66, 0.55, 0.98, 1.0];
const TAG_BG: [f32; 4] = [0.66, 0.55, 0.98, 0.15];
const CODE_BG: [f32; 4] = [1.0, 1.0, 1.0, 0.06];
const HIGHLIGHT_BG: [f32; 4] = [1.0, 0.82, 0.0, 0.40];
const RULE: [f32; 4] = [1.0, 1.0, 1.0, 0.12];
const QUOTE_BAR: [f32; 4] = [0.66, 0.55, 0.98, 1.0];

/// Indent of a list level, and of a quote's body past its bar.
const INDENT: f32 = 24.0;
const BLOCK_GAP: f32 = 10.0;
const CODE_PAD: f32 = 10.0;

#[derive(Clone, Debug, PartialEq)]
pub enum Hit {
    Link(SpanLink),
    /// A task's checkbox: its source line and current status.
    Task { line: usize, status: char },
}

#[derive(Clone, Debug)]
pub enum Draw {
    Text { text: String, x: f32, y: f32, size: f32, color: [f32; 4], font: String, attrs: TextAttrs },
    Quad { rect: Rect, color: [f32; 4] },
    Round { rect: Rect, radius: f32, color: [f32; 4] },
    Line { x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: [f32; 4] },
    Check { cx: f32, cy: f32, r: f32, checked: bool },
    /// A filled disc: a list bullet. Not a `Round` — at a few px the
    /// squircle corner shape draws a small radius as a square.
    Dot { cx: f32, cy: f32, r: f32, color: [f32; 4] },
}

impl Draw {
    fn top_bottom(&self) -> (f32, f32) {
        match self {
            Draw::Text { y, size, .. } => (*y, y + size * 1.3),
            Draw::Quad { rect, .. } | Draw::Round { rect, .. } => (rect.y, rect.y + rect.height),
            Draw::Line { y1, y2, width, .. } => (y1.min(*y2) - width, y1.max(*y2) + width),
            Draw::Check { cy, r, .. } | Draw::Dot { cy, r, .. } => (cy - r, cy + r),
        }
    }
}

#[derive(Default, Debug)]
pub struct Layout {
    pub draws: Vec<Draw>,
    pub hits: Vec<(Rect, Hit)>,
    /// Each block's (and list item's) first source line and the y it
    /// starts at, in source order.
    pub lines: Vec<(usize, f32)>,
    pub height: f32,
}

impl Layout {
    /// The y of the last block starting at or before source `line`.
    pub fn y_of_line(&self, line: usize) -> f32 {
        self.lines.iter().take_while(|(l, _)| *l <= line).last().map(|(_, y)| *y).unwrap_or(0.0)
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<&Hit> {
        self.hits
            .iter()
            .find(|(r, _)| x >= r.x && x <= r.x + r.width && y >= r.y && y <= r.y + r.height)
            .map(|(_, h)| h)
    }

    /// Paint at `origin` scrolled by `scroll`, culled to `viewport`.
    pub fn paint(&self, pc: &mut PaintCtx, origin: (f32, f32), scroll: f32, viewport: Rect) {
        let (ox, oy) = (origin.0, origin.1 - scroll);
        let (top, bottom) = (viewport.y - oy, viewport.y + viewport.height - oy);
        for d in &self.draws {
            let (t, b) = d.top_bottom();
            if b < top || t > bottom {
                continue;
            }
            match d {
                Draw::Text { text, x, y, size, color, font, attrs } => pc.text_attrs(
                    text.clone(),
                    x + ox,
                    y + oy,
                    *size,
                    srgb_u8(*color),
                    Some(font.clone()),
                    None,
                    *attrs,
                ),
                Draw::Quad { rect, color } => pc.quad(shift(*rect, ox, oy), *color),
                Draw::Round { rect, radius, color } => {
                    pc.rounded_rect(shift(*rect, ox, oy), *radius, (true, true, true, true), *color)
                }
                Draw::Line { x1, y1, x2, y2, width, color } => {
                    pc.vector(x1 + ox, y1 + oy, x2 + ox, y2 + oy, *width, *color, Cap::Flat)
                }
                Draw::Check { cx, cy, r, checked } => {
                    cce_ui::widget::Checkbox::paint_round_mark(pc, cx + ox, cy + oy, *r, *checked)
                }
                Draw::Dot { cx, cy, r, color } => pc.circle(cx + ox, cy + oy, *r, *color),
            }
        }
    }
}

/// An sRGB colour (alpha untouched) in the linear space prims take.
fn lin(srgb: [f32; 4]) -> [f32; 4] {
    cce_ui::colors::to_linear(srgb)
}

fn shift(r: Rect, dx: f32, dy: f32) -> Rect {
    Rect { x: r.x + dx, y: r.y + dy, width: r.width, height: r.height }
}

pub fn srgb_u8(linear: [f32; 4]) -> [u8; 3] {
    let s = cce_ui::colors::to_srgb(linear);
    [(s[0] * 255.0) as u8, (s[1] * 255.0) as u8, (s[2] * 255.0) as u8]
}

/// Lay out `blocks` at `width`. `resolved` says whether a note link has a
/// target, so unresolved ones draw faded as in Obsidian.
pub fn layout(
    blocks: &[Block],
    width: f32,
    theme: &Theme,
    m: &mut dyn Measure,
    resolved: &dyn Fn(&SpanLink) -> bool,
) -> Layout {
    let mut l = Layouter { theme, m, resolved, out: Layout::default(), pending: Vec::new() };
    let y = l.blocks(blocks, 0.0, width.max(80.0), 0.0, true);
    l.out.height = y;
    l.out
}

struct Layouter<'a> {
    theme: &'a Theme,
    m: &'a mut dyn Measure,
    resolved: &'a dyn Fn(&SpanLink) -> bool,
    out: Layout,
    /// Runs finished on the current line, emitted when the line ends.
    pending: Vec<Run>,
}

/// One inline run's look, before it is placed.
#[derive(Clone, Copy)]
struct RunStyle {
    size: f32,
    bold: bool,
    color: Option<[f32; 4]>,
}

impl<'a> Layouter<'a> {
    fn blocks(&mut self, blocks: &[Block], x: f32, w: f32, mut y: f32, record: bool) -> f32 {
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 {
                y += BLOCK_GAP;
            }
            if record {
                if let Some(line) = block_line(b) {
                    self.out.lines.push((line, y));
                }
            }
            y = self.block(b, x, w, y);
        }
        y
    }

    fn block(&mut self, b: &Block, x: f32, w: f32, y: f32) -> f32 {
        let size = self.theme.size;
        match b {
            Block::Properties(props) => self.properties(props, x, w, y),
            Block::Heading { level, spans, .. } => {
                let scale = match level {
                    1 => 1.6,
                    2 => 1.4,
                    3 => 1.25,
                    4 => 1.1,
                    _ => 1.0,
                };
                let y = y + if *level <= 2 { size * 0.4 } else { 0.0 };
                let style = RunStyle { size: (size * scale).round(), bold: true, color: None };
                let end = self.inline(spans, x, w, y, style);
                if *level <= 2 {
                    let ry = end + 4.0;
                    self.out.draws.push(Draw::Quad { rect: Rect { x, y: ry, width: w, height: 1.0 }, color: RULE });
                    return ry + 1.0;
                }
                end
            }
            Block::Paragraph { spans, .. } => self.inline(spans, x, w, y, self.plain_style()),
            Block::Embed { target, subpath, .. } => {
                let shown = match subpath {
                    Some(s) => format!("↳ {target}#{s}"),
                    None => format!("↳ {target}"),
                };
                let span = Span {
                    text: shown,
                    style: Style::default(),
                    link: Some(SpanLink::Note { target: target.clone(), subpath: subpath.clone() }),
                };
                self.inline(std::slice::from_ref(&span), x, w, y, self.plain_style())
            }
            Block::List { start, items, .. } => self.list(*start, items, x, w, y),
            Block::Quote { callout, blocks, .. } => self.quote(callout.as_ref(), blocks, x, w, y),
            Block::Code { lang, text, .. } => self.code(lang.as_deref(), text, x, w, y),
            Block::Table { header, rows, .. } => self.table(header, rows, x, w, y),
            Block::Rule { .. } => {
                let ry = y + size * 0.5;
                self.out.draws.push(Draw::Quad { rect: Rect { x, y: ry, width: w, height: 1.0 }, color: RULE });
                ry + size * 0.5
            }
        }
    }

    fn plain_style(&self) -> RunStyle {
        RunStyle { size: self.theme.size, bold: false, color: None }
    }

    fn properties(&mut self, props: &[(String, String)], x: f32, w: f32, mut y: f32) -> f32 {
        let size = (self.theme.size * 0.9).round();
        let key_w = 140.0f32.min(w * 0.35);
        for (k, v) in props {
            let lh = self.theme.line_h(size);
            self.text(k, x, y + (lh - size) / 2.0, size, DIM, false, false);
            let span = Span { text: v.clone(), style: Style::default(), link: None };
            let style = RunStyle { size, bold: false, color: Some(FG) };
            y = self.inline(std::slice::from_ref(&span), x + key_w, w - key_w, y, style);
        }
        y += 6.0;
        self.out.draws.push(Draw::Quad { rect: Rect { x, y, width: w, height: 1.0 }, color: RULE });
        y + 1.0
    }

    fn list(&mut self, start: Option<u64>, items: &[ListItem], x: f32, w: f32, mut y: f32) -> f32 {
        let size = self.theme.size;
        let lh = self.theme.line_h(size);
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                y += 2.0;
            }
            // Items are where a search hit or a task usually points.
            if self.out.lines.last().is_none_or(|(l, _)| *l < item.line) {
                self.out.lines.push((item.line, y));
            }
            let body_x = x + INDENT;
            let mid = y + lh / 2.0;
            match (item.task, start) {
                (Some(status), _) => {
                    let r = (size * 0.45).round();
                    let cx = x + INDENT - r - 6.0;
                    self.out.draws.push(Draw::Check { cx, cy: mid, r, checked: status != ' ' });
                    self.out.hits.push((
                        Rect { x: cx - r - 3.0, y: mid - r - 3.0, width: 2.0 * r + 6.0, height: 2.0 * r + 6.0 },
                        Hit::Task { line: item.line, status },
                    ));
                }
                (None, Some(n)) => {
                    let label = format!("{}.", n + i as u64);
                    let lw = self.m.width(&label, size, &self.theme.body_font, TextAttrs::default());
                    self.text(&label, body_x - 6.0 - lw, y + (lh - size) / 2.0, size, DIM, false, false);
                }
                (None, None) => {
                    let r = (size * 0.16).max(2.0);
                    self.out.draws.push(Draw::Dot { cx: body_x - 12.0, cy: mid, r, color: DIM });
                }
            }
            let done = item.task.is_some_and(|s| s == 'x' || s == 'X');
            let before = self.out.draws.len();
            y = self.item_blocks(&item.blocks, body_x, w - INDENT, y);
            if done {
                // Obsidian dims a finished task's text rather than striking it.
                for d in &mut self.out.draws[before..] {
                    if let Draw::Text { color, .. } = d {
                        *color = DIM;
                    }
                }
            }
        }
        y
    }

    /// An item's blocks, without the block gap before a nested list.
    fn item_blocks(&mut self, blocks: &[Block], x: f32, w: f32, mut y: f32) -> f32 {
        let lh = self.theme.line_h(self.theme.size);
        if blocks.is_empty() {
            return y + lh;
        }
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 && !matches!(b, Block::List { .. }) {
                y += BLOCK_GAP / 2.0;
            }
            y = self.block(b, x, w, y);
        }
        y
    }

    fn quote(&mut self, callout: Option<&Callout>, blocks: &[Block], x: f32, w: f32, y: f32) -> f32 {
        let size = self.theme.size;
        let start = self.out.draws.len();
        let pad = if callout.is_some() { 10.0 } else { 0.0 };
        let mut cy = y + pad;
        let body_x = x + if callout.is_some() { 14.0 } else { INDENT * 0.6 };
        let body_w = w - (body_x - x) - pad;
        if let Some(c) = callout {
            let style = RunStyle { size, bold: true, color: Some(lin(callout_color(&c.kind))) };
            let title: Vec<Span> = if c.title.is_empty() {
                vec![Span { text: title_case(&c.kind), style: Style::default(), link: None }]
            } else {
                c.title.clone()
            };
            cy = self.inline(&title, body_x, body_w, cy, style);
            if !blocks.is_empty() && c.folded != Some(true) {
                cy += BLOCK_GAP / 2.0;
            }
        }
        if callout.is_none_or(|c| c.folded != Some(true)) {
            cy = self.blocks(blocks, body_x, body_w, cy, false);
        }
        let end = cy + pad;
        let rect = Rect { x, y, width: w, height: end - y };
        // Backgrounds go under the text laid out above them.
        match callout {
            Some(c) => {
                let mut bg = lin(callout_color(&c.kind));
                bg[3] = 0.10;
                self.out.draws.insert(start, Draw::Round { rect, radius: 6.0, color: bg });
            }
            None => {
                self.out.draws.insert(
                    start,
                    Draw::Quad { rect: Rect { x, y, width: 2.0, height: end - y }, color: lin(QUOTE_BAR) },
                );
            }
        }
        end
    }

    fn code(&mut self, _lang: Option<&str>, text: &str, x: f32, w: f32, y: f32) -> f32 {
        let size = (self.theme.size * 0.9).round();
        let lh = self.theme.line_h(size);
        let n = text.split('\n').count().max(1);
        let h = n as f32 * lh + 2.0 * CODE_PAD;
        self.out.draws.push(Draw::Round { rect: Rect { x, y, width: w, height: h }, radius: 6.0, color: CODE_BG });
        // Code does not wrap; overlong lines are cut at the block's edge
        // by the paint clip, as a scrolled source view would show them.
        for (i, line) in text.split('\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let ty = y + CODE_PAD + i as f32 * lh + (lh - size) / 2.0;
            let font = self.theme.mono_font.clone();
            self.out.draws.push(Draw::Text {
                text: line.replace('\t', "    "),
                x: x + CODE_PAD,
                y: ty,
                size,
                color: FG,
                font,
                attrs: TextAttrs::default(),
            });
        }
        y + h
    }

    fn table(&mut self, header: &[Vec<Span>], rows: &[Vec<Vec<Span>>], x: f32, w: f32, mut y: f32) -> f32 {
        let cols = header.len().max(rows.iter().map(Vec::len).max().unwrap_or(0)).max(1);
        let col_w = w / cols as f32;
        let pad = 6.0;
        let top = y;
        let all = std::iter::once((header, true)).chain(rows.iter().map(|r| (r.as_slice(), false)));
        for (ri, (row, is_header)) in all.enumerate() {
            let style = RunStyle { size: self.theme.size, bold: is_header, color: None };
            let mut bottom = y;
            for (ci, cell) in row.iter().enumerate().take(cols) {
                let cx = x + ci as f32 * col_w + pad;
                bottom = bottom.max(self.inline(cell, cx, col_w - 2.0 * pad, y + pad, style));
            }
            y = bottom.max(y + self.theme.line_h(self.theme.size)) + pad;
            if ri == 0 || ri < rows.len() {
                self.out.draws.push(Draw::Quad { rect: Rect { x, y, width: w, height: 1.0 }, color: RULE });
            }
        }
        for c in 1..cols {
            let lx = x + c as f32 * col_w;
            self.out.draws.push(Draw::Quad { rect: Rect { x: lx, y: top, width: 1.0, height: y - top }, color: RULE });
        }
        y
    }

    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, text: &str, x: f32, y: f32, size: f32, color: [f32; 4], bold: bool, italic: bool) {
        let attrs = TextAttrs { italic, weight: bold.then_some(700) };
        let font = self.theme.body_font.clone();
        self.out.draws.push(Draw::Text { text: text.to_string(), x, y, size, color, font, attrs });
    }

    /// Lay out inline spans from `(x, y)` wrapping at `x + w`; returns the
    /// y below the last line.
    fn inline(&mut self, spans: &[Span], x: f32, w: f32, y: f32, base: RunStyle) -> f32 {
        let lh = self.theme.line_h(base.size);
        let right = x + w;
        let looks: Vec<Look> = spans.iter().map(|s| self.look(s, base)).collect();
        let spaces: Vec<f32> = looks.iter().map(|l| self.space_width(l)).collect();
        let mut cx = x;
        let mut line_y = y;
        // A space seen since the last placed word, in the style it was in.
        let mut gap: Option<f32> = None;
        let mut run: Option<Run> = None;
        let mut any = false;

        for tok in tokens(spans) {
            match tok {
                Tok::Break => {
                    self.flush(&mut run, line_y, lh);
                    line_y += lh;
                    cx = x;
                    gap = None;
                }
                Tok::Space(si) => {
                    if cx > x {
                        gap = Some(spaces[si]);
                    }
                }
                Tok::Word(segs) => {
                    any = true;
                    let widths: Vec<f32> = segs
                        .iter()
                        .map(|(t, si)| {
                            let l = &looks[*si];
                            self.m.width(t, l.size, &l.font, l.attrs)
                        })
                        .collect();
                    let total: f32 = widths.iter().sum();
                    let mut lead = gap.take().unwrap_or(0.0);
                    if cx > x && cx + lead + total > right {
                        self.flush(&mut run, line_y, lh);
                        line_y += lh;
                        cx = x;
                        lead = 0.0;
                    }
                    for ((text, si), mut ww) in segs.into_iter().zip(widths) {
                        let (look, span) = (&looks[si], &spans[si]);
                        // A word wider than the whole line (a URL) breaks
                        // by characters rather than overflowing.
                        let mut rest = text;
                        while cx + ww > right && rest.chars().count() > 1 {
                            let cut = self.fit(rest, right - cx, look);
                            let (head, tail) = rest.split_at(cut);
                            let hw = self.m.width(head, look.size, &look.font, look.attrs);
                            self.place(&mut run, head, cx + lead, hw, lead, look, span);
                            self.flush(&mut run, line_y, lh);
                            line_y += lh;
                            cx = x;
                            lead = 0.0;
                            rest = tail;
                            ww = self.m.width(rest, look.size, &look.font, look.attrs);
                        }
                        self.place(&mut run, rest, cx + lead, ww, lead, look, span);
                        cx += lead + ww;
                        lead = 0.0;
                    }
                }
            }
        }
        self.flush(&mut run, line_y, lh);
        if !any && line_y == y {
            return y + lh;
        }
        line_y + lh
    }

    /// The longest char prefix of `word` fitting `w` (at least one char).
    fn fit(&mut self, word: &str, w: f32, look: &Look) -> usize {
        let mut best = word.chars().next().map(char::len_utf8).unwrap_or(word.len());
        for (i, c) in word.char_indices().skip(1) {
            let end = i + c.len_utf8();
            if self.m.width(&word[..end], look.size, &look.font, look.attrs) > w {
                break;
            }
            best = end;
        }
        best
    }

    fn space_width(&mut self, look: &Look) -> f32 {
        let two = self.m.width("x x", look.size, &look.font, look.attrs);
        let one = self.m.width("xx", look.size, &look.font, look.attrs);
        (two - one).max(look.size * 0.2)
    }

    fn look(&self, span: &Span, base: RunStyle) -> Look {
        let s = span.style;
        let code = s.code || s.math;
        let size = if code { (base.size * 0.92).round() } else { base.size };
        let color = match &span.link {
            Some(SpanLink::Tag(_)) => lin(TAG),
            Some(l @ (SpanLink::Note { .. } | SpanLink::Embed { .. })) => {
                if (self.resolved)(l) {
                    lin(LINK)
                } else {
                    lin(LINK_UNRESOLVED)
                }
            }
            Some(SpanLink::Url(_)) => lin(LINK),
            None => base.color.unwrap_or(FG),
        };
        Look {
            size,
            font: if code { self.theme.mono_font.clone() } else { self.theme.body_font.clone() },
            attrs: TextAttrs { italic: s.italic, weight: (s.bold || base.bold).then_some(700) },
            color,
            bg: if code {
                Some(CODE_BG)
            } else if s.highlight {
                Some(lin(HIGHLIGHT_BG))
            } else if matches!(span.link, Some(SpanLink::Tag(_))) {
                Some(lin(TAG_BG))
            } else {
                None
            },
            strike: s.strike,
        }
    }

    /// Add a word to the current run, or start a new one if the look or
    /// link changed.
    #[allow(clippy::too_many_arguments)]
    fn place(&mut self, run: &mut Option<Run>, word: &str, x: f32, w: f32, lead: f32, look: &Look, span: &Span) {
        if let Some(r) = run {
            if r.look == *look && r.link == span.link && (r.x + r.w + lead - x).abs() < 0.5 {
                if lead > 0.0 {
                    r.text.push(' ');
                }
                r.text.push_str(word);
                r.w += lead + w;
                return;
            }
        }
        // The line is the same until flushed; only a look change ends it.
        if let Some(r) = run.take() {
            self.pending.push(r);
        }
        *run = Some(Run { text: word.to_string(), x, w, look: look.clone(), link: span.link.clone() });
    }

    fn flush(&mut self, run: &mut Option<Run>, line_y: f32, lh: f32) {
        if let Some(r) = run.take() {
            self.pending.push(r);
        }
        for r in std::mem::take(&mut self.pending) {
            let ty = line_y + (lh - r.look.size) / 2.0;
            let box_ = Rect { x: r.x - 2.0, y: line_y + 2.0, width: r.w + 4.0, height: lh - 4.0 };
            if let Some(bg) = r.look.bg {
                self.out.draws.push(Draw::Round { rect: box_, radius: 3.0, color: bg });
            }
            self.out.draws.push(Draw::Text {
                text: r.text,
                x: r.x,
                y: ty,
                size: r.look.size,
                color: r.look.color,
                font: r.look.font.clone(),
                attrs: r.look.attrs,
            });
            if r.look.strike {
                let sy = ty + r.look.size * 0.55;
                self.out.draws.push(Draw::Line { x1: r.x, y1: sy, x2: r.x + r.w, y2: sy, width: 1.0, color: r.look.color });
            }
            if let Some(link) = r.link {
                self.out.hits.push((Rect { x: r.x, y: line_y, width: r.w, height: lh }, Hit::Link(link)));
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Look {
    size: f32,
    font: String,
    attrs: TextAttrs,
    color: [f32; 4],
    bg: Option<[f32; 4]>,
    strike: bool,
}

struct Run {
    text: String,
    x: f32,
    w: f32,
    look: Look,
    link: Option<SpanLink>,
}

enum Piece<'a> {
    Word(&'a str),
    Space,
    Break,
}

/// Split text into words, whitespace runs and hard line breaks.
fn pieces(text: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push(Piece::Word(&text[s..i]));
            }
            if c == '\n' {
                out.push(Piece::Break);
            } else if !matches!(out.last(), Some(Piece::Space)) {
                out.push(Piece::Space);
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push(Piece::Word(&text[s..]));
    }
    out
}

/// What wraps as a unit: a word may run across spans (`**bold**,` is one
/// word in two styles), and never breaks between them.
enum Tok<'a> {
    /// Segments of one word, each with the index of its span.
    Word(Vec<(&'a str, usize)>),
    /// A space, in the style of the span it sits in.
    Space(usize),
    Break,
}

fn tokens(spans: &[Span]) -> Vec<Tok<'_>> {
    let mut out: Vec<Tok> = Vec::new();
    for (si, span) in spans.iter().enumerate() {
        for (pi, piece) in pieces(&span.text).into_iter().enumerate() {
            match piece {
                Piece::Word(w) => match out.last_mut() {
                    // Glued: the previous span ended mid-word.
                    Some(Tok::Word(segs)) if pi == 0 => segs.push((w, si)),
                    _ => out.push(Tok::Word(vec![(w, si)])),
                },
                Piece::Space => out.push(Tok::Space(si)),
                Piece::Break => out.push(Tok::Break),
            }
        }
    }
    out
}

fn block_line(b: &Block) -> Option<usize> {
    match b {
        Block::Properties(_) => Some(0),
        Block::Heading { line, .. }
        | Block::Paragraph { line, .. }
        | Block::Embed { line, .. }
        | Block::List { line, .. }
        | Block::Quote { line, .. }
        | Block::Code { line, .. }
        | Block::Table { line, .. }
        | Block::Rule { line } => Some(*line),
    }
}

fn callout_color(kind: &str) -> [f32; 4] {
    match kind {
        "warning" | "caution" | "attention" => [0.93, 0.60, 0.25, 1.0],
        "danger" | "error" | "bug" | "failure" | "fail" | "missing" => [0.93, 0.35, 0.35, 1.0],
        "tip" | "hint" | "important" | "success" | "check" | "done" => [0.30, 0.78, 0.60, 1.0],
        "question" | "help" | "faq" => [0.93, 0.75, 0.25, 1.0],
        "quote" | "cite" => [0.60, 0.60, 0.65, 1.0],
        "example" => [0.62, 0.55, 0.98, 1.0],
        _ => [0.35, 0.62, 0.95, 1.0],
    }
}

fn title_case(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cce_vault::markdown::blocks;

    /// Every char 10 px at size 10, scaled with the size.
    struct Fixed;
    impl Measure for Fixed {
        fn width(&mut self, text: &str, size: f32, _font: &str, _attrs: TextAttrs) -> f32 {
            text.chars().count() as f32 * size
        }
    }

    fn theme() -> Theme {
        Theme { body_font: "sans-serif".into(), mono_font: "monospace".into(), size: 10.0 }
    }

    fn texts(l: &Layout) -> Vec<(String, f32, f32)> {
        l.draws
            .iter()
            .filter_map(|d| match d {
                Draw::Text { text, x, y, .. } => Some((text.clone(), *x, *y)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn wraps_words_and_merges_runs() {
        // "aaa bbb ccc" at 10px/char: 110 px; at width 80 the third word wraps.
        let doc = blocks("aaa bbb ccc\n");
        let l = layout(&doc, 80.0, &theme(), &mut Fixed, &|_| true);
        let t = texts(&l);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[0].0, "aaa bbb");
        assert_eq!(t[1].0, "ccc");
        assert_eq!(t[1].1, 0.0);
        assert!(t[1].2 > t[0].2);
        assert_eq!(l.height, 30.0);
    }

    #[test]
    fn styles_split_runs_and_links_hit() {
        let doc = blocks("go **bold** to [[Note]] now\n");
        let l = layout(&doc, 1000.0, &theme(), &mut Fixed, &|_| true);
        let t: Vec<String> = texts(&l).into_iter().map(|t| t.0).collect();
        assert_eq!(t, ["go", "bold", "to", "Note", "now"]);
        let (r, hit) = &l.hits[0];
        assert_eq!(hit, &Hit::Link(SpanLink::Note { target: "Note".into(), subpath: None }));
        assert!(l.hit(r.x + 1.0, r.y + 1.0).is_some());
        assert!(l.hit(0.0, 1.0).is_none());
    }

    #[test]
    fn punctuation_glued_to_a_styled_word_wraps_with_it() {
        // Code is 9 px/char. At width 80 (layout's minimum) "aaaa bbb"
        // fits (77 px) but "aaaa bbb," does not (87): unglued, the comma
        // alone would start the next line.
        let doc = blocks("aaaa `bbb`, cc\n");
        let l = layout(&doc, 80.0, &theme(), &mut Fixed, &|_| true);
        let t = texts(&l);
        let bbb = t.iter().find(|t| t.0 == "bbb").unwrap();
        let comma = t.iter().find(|t| t.0.starts_with(',')).unwrap();
        assert_eq!((bbb.1, comma.1), (0.0, 27.0), "{t:?}");
        assert!(bbb.2 > t[0].2, "{t:?}");
    }

    #[test]
    fn long_word_breaks_by_chars() {
        let doc = blocks("abcdefghij\n");
        let l = layout(&doc, 80.0, &theme(), &mut Fixed, &|_| true);
        let t: Vec<String> = texts(&l).into_iter().map(|t| t.0).collect();
        assert_eq!(t, ["abcdefgh", "ij"]);
    }

    #[test]
    fn tasks_hit_with_their_line_and_blocks_map_lines() {
        let doc = blocks("# Head\n\n- [ ] one\n- [x] two\n\nend\n");
        let l = layout(&doc, 400.0, &theme(), &mut Fixed, &|_| true);
        let tasks: Vec<_> = l.hits.iter().filter_map(|(_, h)| match h {
            Hit::Task { line, status } => Some((*line, *status)),
            _ => None,
        }).collect();
        assert_eq!(tasks, [(2, ' '), (3, 'x')]);
        assert_eq!(l.lines.iter().map(|(l, _)| *l).collect::<Vec<_>>(), [0, 2, 3, 5]);
        assert!(l.y_of_line(5) > l.y_of_line(2));
    }

    #[test]
    fn hard_breaks_and_empty_input() {
        let doc = blocks("one\ntwo\n");
        let l = layout(&doc, 400.0, &theme(), &mut Fixed, &|_| true);
        assert_eq!(texts(&l).len(), 2);
        assert_eq!(layout(&[], 400.0, &theme(), &mut Fixed, &|_| true).height, 0.0);
    }
}
