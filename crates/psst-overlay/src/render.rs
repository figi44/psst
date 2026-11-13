//! Text is rasterised at the physical size rather than through the transform, so it stays sharp
//! at fractional scales.

use std::sync::Arc;

use cosmic_text::{
    Attrs, Buffer, Ellipsize, EllipsizeHeightLimit, Family, FontSystem, LayoutGlyph, Metrics, Shaping, SwashCache,
    SwashContent, Wrap, fontdb,
};
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Stroke, Transform};

use crate::menu::{Ink, Lead, Line, Mark, Menu, PAD, ROW};
use crate::{OverlayState, Rgba, Style, Wave, physical_size};

/// Inter 4.1 Medium, SIL Open Font License 1.1 (`fonts/Inter-LICENSE.txt`).
static INTER: &[u8] = include_bytes!("../fonts/Inter-Medium.ttf");
const FAMILY: &str = "Inter";

const SUCCESS: Rgba = Rgba::new(0x3d, 0xd6, 0x8c, 0xff);
const WARNING: Rgba = Rgba::new(0xff, 0xb0, 0x3b, 0xff);
const ERROR: Rgba = Rgba::new(0xff, 0x5f, 0x52, 0xff);

const MENU_TEXT: f32 = 14.0;

/// Building one loads the font, so build one and keep it.
pub struct Renderer {
    fonts: FontSystem,
    glyphs: SwashCache,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        db.load_font_source(fontdb::Source::Binary(Arc::new(INTER)));
        db.set_sans_serif_family(FAMILY);
        // A fixed locale keeps shaping independent of the machine.
        let fonts = FontSystem::new_with_locale_and_db("en-US".into(), db);
        Self { fonts, glyphs: SwashCache::new() }
    }

    pub fn render(
        &mut self,
        state: &OverlayState,
        wave: Wave,
        style: &Style,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Pixmap {
        let (pw, ph) = physical_size(width, height, scale);
        let pixmap = Pixmap::new(pw, ph).expect("non-zero size");
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        let mut frame = Frame { pixmap, w, h, sx: pw as f32 / w, sy: ph as f32 / h, renderer: self, style };
        frame.draw(state, wave);
        frame.pixmap
    }

    pub fn render_menu(&mut self, menu: &Menu, style: &Style, scale: f64) -> Pixmap {
        let (width, height) = menu.size();
        let (pw, ph) = physical_size(width, height, scale);
        let pixmap = Pixmap::new(pw, ph).expect("non-zero size");
        let (w, h) = (width as f32, height as f32);
        let mut frame = Frame { pixmap, w, h, sx: pw as f32 / w, sy: ph as f32 / h, renderer: self, style };
        frame.menu(menu);
        frame.pixmap
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
}

/// Coordinates are logical pixels unless named `p*`.
struct Frame<'a> {
    pixmap: Pixmap,
    w: f32,
    h: f32,
    sx: f32,
    sy: f32,
    renderer: &'a mut Renderer,
    style: &'a Style,
}

enum Content<'s> {
    Label { text: &'s str, color: Rgba },
    Waveform { color: Rgba },
    WaveAndText { bars: Rgba, text: &'s str, color: Rgba, align: Align },
}

#[derive(Clone, Copy)]
enum Icon {
    Dot(Rgba),
    Ring(Rgba),
    Arc,
    Dots,
    Arrow,
    Retry,
    Clock,
    Flat,
    Check,
    Bang,
    Info,
}

impl Frame<'_> {
    fn draw(&mut self, state: &OverlayState, wave: Wave) {
        let fg = self.style.foreground;
        let muted = fg.faded(0.62);
        let accent = self.style.accent;
        let countdown;
        let (tint, icon, content) = match state {
            OverlayState::Starting => {
                (None, Icon::Ring(accent), Content::Label { text: "Starting\u{2026}", color: muted })
            }
            OverlayState::Recording { .. } => (
                Some(accent.faded(0.16 + 0.5 * loudness(wave.level))),
                Icon::Dot(accent),
                Content::Waveform { color: fg },
            ),
            OverlayState::Offline { reason, .. } => (
                Some(WARNING.faded(0.18)),
                Icon::Dot(WARNING),
                Content::WaveAndText { bars: fg.faded(0.8), text: reason, color: muted, align: Align::Left },
            ),
            OverlayState::LoadingModel => {
                (None, Icon::Arc, Content::Label { text: "Loading model\u{2026}", color: fg })
            }
            OverlayState::Processing => (None, Icon::Dots, Content::Label { text: "Transcribing\u{2026}", color: fg }),
            OverlayState::Sending => (None, Icon::Arrow, Content::Label { text: "Sending\u{2026}", color: fg }),
            OverlayState::Retrying => (None, Icon::Retry, Content::Label { text: "Retrying\u{2026}", color: fg }),
            OverlayState::WaitingForServer => {
                (None, Icon::Clock, Content::Label { text: "Waiting for server\u{2026}", color: fg })
            }
            OverlayState::NothingHeard => (None, Icon::Flat, Content::Label { text: "Nothing heard", color: muted }),
            OverlayState::Done => (Some(SUCCESS.faded(0.2)), Icon::Check, Content::Label { text: "Done", color: fg }),
            OverlayState::Error { message } => {
                (Some(ERROR.faded(0.22)), Icon::Bang, Content::Label { text: message, color: fg })
            }
            OverlayState::Flash { message } => (None, Icon::Info, Content::Label { text: message, color: fg }),
            OverlayState::LimitWarning { seconds_left, .. } => {
                countdown = format_countdown(*seconds_left);
                (
                    Some(WARNING.faded(0.2)),
                    Icon::Dot(WARNING),
                    Content::WaveAndText { bars: fg, text: &countdown, color: WARNING, align: Align::Right },
                )
            }
        };

        self.pill();
        self.well(tint);
        self.icon(icon);
        let (x0, x1) = (self.h + 1.0, self.w - self.h * 0.45);
        match content {
            Content::Label { text, color } => {
                self.text(text, x0, x1 - x0, color, Align::Left);
            }
            Content::Waveform { color } => self.waveform(wave, x0, x1, color),
            Content::WaveAndText { bars, text, color, align: Align::Left } => {
                let split = x0 + ((x1 - x0) * 0.3).max(self.h);
                self.waveform(wave, x0, split, bars);
                self.text(text, split + self.h * 0.25, x1 - split - self.h * 0.25, color, Align::Left);
            }
            Content::WaveAndText { bars, text, color, align: Align::Right } => {
                let used = self.text(text, x0, x1 - x0, color, Align::Right);
                self.waveform(wave, x0, x1 - used - self.h * 0.3, bars);
            }
        }
    }

    fn transform(&self) -> Transform {
        Transform::from_scale(self.sx, self.sy)
    }

    fn fill(&mut self, path: &Path, color: Rgba) {
        let paint = paint(color);
        let t = self.transform();
        self.pixmap.fill_path(path, &paint, FillRule::Winding, t, None);
    }

    fn stroke(&mut self, path: &Path, color: Rgba, width: f32) {
        let paint = paint(color);
        let stroke = Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() };
        let t = self.transform();
        self.pixmap.stroke_path(path, &paint, &stroke, t, None);
    }

    fn pill(&mut self) {
        self.panel(self.h / 2.0);
    }

    /// The faint hairline edge makes it read on dark and light windows alike.
    fn panel(&mut self, radius: f32) {
        let (w, h) = (self.w, self.h);
        if let Some(p) = rounded_rect(0.0, 0.0, w, h, radius) {
            self.fill(&p, self.style.background);
        }
        let hair = 1.0 / self.sy;
        if let Some(p) = rounded_rect(hair / 2.0, hair / 2.0, w - hair, h - hair, radius - hair / 2.0) {
            self.stroke(&p, self.style.foreground.faded(0.10), hair);
        }
    }

    fn menu(&mut self, menu: &Menu) {
        let fg = self.style.foreground;
        let muted = fg.faded(0.62);
        self.panel(14.0);
        let (x0, x1) = (PAD, self.w - PAD);
        let text_x = x0 + 12.0;
        for (i, line) in menu.lines().iter().enumerate() {
            let top = PAD + i as f32 * ROW;
            let cy = top + ROW / 2.0;
            match line {
                Line::Header(title) => {
                    self.polyline(&[(x0 + 15.0, cy - 5.0), (x0 + 10.0, cy), (x0 + 15.0, cy + 5.0)], muted, 1.8);
                    self.text_at(title, (x0 + 24.0, cy), x1 - x0 - 36.0, MENU_TEXT, muted, Align::Left);
                    let y = top + ROW - 1.0;
                    self.polyline(&[(x0 + 6.0, y), (x1 - 6.0, y)], fg.faded(0.08), 1.0 / self.sy);
                }
                Line::Item(item) => {
                    if menu.selected() == Some(i)
                        && let Some(p) = rounded_rect(x0, top + 1.0, x1 - x0, ROW - 2.0, 9.0)
                    {
                        self.fill(&p, fg.faded(0.10));
                    }
                    let color = match item.ink {
                        _ if !item.enabled() => fg.faded(0.4),
                        Ink::Normal => fg,
                        Ink::Quiet => muted,
                        Ink::Alert => self.style.accent,
                    };
                    let lead_x = x0 + 14.0;
                    match item.lead {
                        Lead::Dot => self.circle((lead_x, cy), 3.5, self.style.accent),
                        Lead::Ring => {
                            if let Some(ring) = PathBuilder::from_circle(lead_x, cy, 3.0) {
                                self.stroke(&ring, muted, 1.4);
                            }
                        }
                        Lead::None | Lead::Space => {}
                    }
                    let text_x = if item.lead == Lead::None { text_x } else { text_x + 14.0 };
                    let mut right = x1 - 12.0;
                    match item.mark {
                        Mark::Chevron => {
                            self.polyline(
                                &[(right - 4.0, cy - 5.0), (right + 1.0, cy), (right - 4.0, cy + 5.0)],
                                muted,
                                1.8,
                            );
                            right -= 16.0;
                        }
                        Mark::Check => {
                            self.polyline(&[(right - 11.0, cy), (right - 7.0, cy + 4.0), (right, cy - 4.5)], fg, 2.0);
                            right -= 20.0;
                        }
                        Mark::None => {}
                    }
                    let detail = match &item.detail {
                        Some(detail) => {
                            let room = (right - text_x) * 0.5;
                            self.text_at(detail, (right - room, cy), room, MENU_TEXT, muted, Align::Right) + 10.0
                        }
                        None => 0.0,
                    };
                    self.text_at(&item.label, (text_x, cy), right - detail - text_x, MENU_TEXT, color, Align::Left);
                }
                Line::Note(text) => {
                    self.text_at(text, (text_x, cy), x1 - 12.0 - text_x, MENU_TEXT, muted, Align::Left);
                }
                Line::Field => {
                    if let Some(p) = rounded_rect(x0, top + 2.0, x1 - x0, ROW - 4.0, 9.0) {
                        self.fill(&p, fg.faded(0.07));
                    }
                    let (word, caret) = menu.word();
                    let room = x1 - 12.0 - text_x;
                    if word.is_empty() {
                        let hint = "The word as it should be written";
                        self.text_at(hint, (text_x + 4.0, cy), room - 4.0, MENU_TEXT, fg.faded(0.4), Align::Left);
                    }
                    self.field(word, caret, (text_x, cy), room, fg);
                }
            }
        }
    }

    fn well_radius(&self) -> f32 {
        self.h * 0.36
    }

    fn well(&mut self, tint: Option<Rgba>) {
        let c = self.h / 2.0;
        let fill = tint.unwrap_or(self.style.foreground.faded(0.07));
        if let Some(p) = PathBuilder::from_circle(c, c, self.well_radius()) {
            self.fill(&p, fill);
        }
    }

    /// Icon coordinates are in a 32-unit box centred in the well.
    fn icon(&mut self, icon: Icon) {
        let c = self.h / 2.0;
        let u = self.well_radius() / 16.0;
        let at = |x: f32, y: f32| (c + x * u, c + y * u);
        let fg = self.style.foreground;
        let line = 2.0 * u;
        match icon {
            Icon::Dot(color) => self.circle(at(0.0, 0.0), 5.5 * u, color),
            Icon::Ring(color) => {
                if let Some(p) = PathBuilder::from_circle(c, c, 5.5 * u) {
                    self.stroke(&p, color, line);
                }
            }
            Icon::Arc => {
                let mut pb = PathBuilder::new();
                arc(&mut pb, c, c, 6.0 * u, -90.0, 180.0);
                if let Some(p) = pb.finish() {
                    self.stroke(&p, fg, line);
                }
            }
            Icon::Dots => {
                for (i, alpha) in [0.4, 0.7, 1.0].into_iter().enumerate() {
                    self.circle(at(-5.5 + 5.5 * i as f32, 0.0), 1.9 * u, fg.faded(alpha));
                }
            }
            Icon::Arrow => {
                self.polyline(&[at(0.0, 6.0), at(0.0, -5.5)], fg, line);
                self.polyline(&[at(-4.5, -1.0), at(0.0, -5.5), at(4.5, -1.0)], fg, line);
            }
            Icon::Retry => {
                let (r, end) = (6.0 * u, 300.0_f32);
                let mut pb = PathBuilder::new();
                arc(&mut pb, c, c, r, 30.0, end);
                if let Some(p) = pb.finish() {
                    self.stroke(&p, fg, line);
                }
                let (ex, ey) = polar(c, c, r, end);
                let a = end.to_radians();
                let (dx, dy) = (-a.sin(), a.cos());
                let (head, back) = (3.6 * u, 1.2 * u);
                let mut pb = PathBuilder::new();
                pb.move_to(ex + dx * head, ey + dy * head);
                pb.line_to(ex - dy * head * 0.8 - dx * back, ey + dx * head * 0.8 - dy * back);
                pb.line_to(ex + dy * head * 0.8 - dx * back, ey - dx * head * 0.8 - dy * back);
                pb.close();
                if let Some(p) = pb.finish() {
                    self.fill(&p, fg);
                }
            }
            Icon::Clock => {
                if let Some(p) = PathBuilder::from_circle(c, c, 6.5 * u) {
                    self.stroke(&p, fg, 1.8 * u);
                }
                self.polyline(&[at(0.0, -3.8), at(0.0, 0.0), at(3.0, 1.6)], fg, 1.8 * u);
            }
            Icon::Flat => {
                self.polyline(&[at(-6.0, 0.0), at(6.0, 0.0)], fg.faded(0.62), line);
            }
            Icon::Check => {
                self.polyline(&[at(-5.0, 0.5), at(-1.5, 4.0), at(5.5, -3.8)], SUCCESS, 2.3 * u);
            }
            Icon::Bang => {
                self.polyline(&[at(0.0, -5.5), at(0.0, 1.2)], ERROR, 2.3 * u);
                self.circle(at(0.0, 5.0), 1.35 * u, ERROR);
            }
            Icon::Info => {
                self.circle(at(0.0, -5.0), 1.35 * u, fg);
                self.polyline(&[at(0.0, -1.2), at(0.0, 5.5)], fg, 2.3 * u);
            }
        }
    }

    fn circle(&mut self, (x, y): (f32, f32), r: f32, color: Rgba) {
        if let Some(p) = PathBuilder::from_circle(x, y, r) {
            self.fill(&p, color);
        }
    }

    fn polyline(&mut self, points: &[(f32, f32)], color: Rgba, width: f32) {
        let mut pb = PathBuilder::new();
        let Some((&(x, y), rest)) = points.split_first() else { return };
        pb.move_to(x, y);
        for &(x, y) in rest {
            pb.line_to(x, y);
        }
        if let Some(p) = pb.finish() {
            self.stroke(&p, color, width);
        }
    }

    /// Bars that stay in place: tallest in the middle, bobbing with two standing waves, and
    /// swelling with the level. Silence leaves a slow, low ripple.
    fn waveform(&mut self, wave: Wave, x0: f32, x1: f32, color: Rgba) {
        let bar = (self.h * 0.068).max(2.0);
        let gap = (self.h * 0.08).max(1.5);
        let pitch = bar + gap;
        let n = ((x1 - x0 + gap) / pitch).floor().max(0.0) as usize;
        if n == 0 {
            return;
        }
        let start = x0 + (x1 - x0 - (n as f32 * pitch - gap)) / 2.0;
        let mid = self.h / 2.0;
        let max_h = self.h * 0.62;
        let swell = QUIET_SWELL + (1.0 - QUIET_SWELL) * loudness(wave.level);
        for i in 0..n {
            let p = if n == 1 { 0.0 } else { 2.0 * i as f32 / (n - 1) as f32 - 1.0 };
            let bell = (-(p * 1.6).powi(2)).exp();
            let ripple =
                0.5 + 0.3 * (p * 5.5).cos() * wave.phase.sin() + 0.2 * (p * 9.5).cos() * (0.6 * wave.phase + 1.3).sin();
            let height = bar + (max_h - bar) * swell * (0.15 + 0.85 * bell) * ripple;
            let x = start + i as f32 * pitch;
            if let Some(p) = rounded_rect(x, mid - height / 2.0, bar, height, bar / 2.0) {
                self.fill(&p, color.faded(0.4 + 0.6 * bell));
            }
        }
    }

    fn text(&mut self, text: &str, x: f32, max_w: f32, color: Rgba, align: Align) -> f32 {
        let size = (self.h * 0.31).clamp(9.0, 24.0);
        self.text_at(text, (x, self.h / 2.0), max_w, size, color, align)
    }

    /// One line, ellipsised to `max_w`, with its capitals centred on `cy`. Returns the width used.
    fn text_at(&mut self, text: &str, (x, cy): (f32, f32), max_w: f32, size: f32, color: Rgba, align: Align) -> f32 {
        if max_w <= 0.0 || text.is_empty() {
            return 0.0;
        }
        let sx = self.sx;
        // Newlines would start lines nobody sees; the overlay shows one.
        let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let Some((glyphs, line_w)) = self.shape(&one_line, size, Some(max_w)) else { return 0.0 };
        let width = line_w.min(max_w * sx);
        let px = match align {
            Align::Left => x * sx,
            Align::Right => (x + max_w) * sx - width,
        };
        self.draw_glyphs(&glyphs, (px, self.baseline(cy, size)), (0, self.pixmap.width() as i32), color);
        width / sx
    }

    /// One line that scrolls to keep the caret in view instead of ellipsising.
    fn field(&mut self, text: &str, caret: usize, (x, cy): (f32, f32), max_w: f32, color: Rgba) {
        let (glyphs, line_w) = self.shape(text, MENU_TEXT, None).unwrap_or_default();
        let caret_px = glyphs.iter().find(|g| g.start >= caret).map_or(line_w, |g| g.x);
        let (left, room) = (x * self.sx, max_w * self.sx);
        let shift = (caret_px - room).max(0.0);
        let clip = (left.floor() as i32, (left + room).ceil() as i32);
        self.draw_glyphs(&glyphs, (left - shift, self.baseline(cy, MENU_TEXT)), clip, color);
        let caret_x = (left - shift + caret_px) / self.sx;
        self.polyline(&[(caret_x, cy - 9.0), (caret_x, cy + 9.0)], color, 1.5);
    }

    /// Centres the capitals on `cy`: Inter's cap height is 0.727 em.
    fn baseline(&self, cy: f32, size: f32) -> f32 {
        ((cy + 0.36 * size) * self.sy).round()
    }

    /// Physical pixels; `max_w` (logical) ellipsises the end.
    fn shape(&mut self, text: &str, size: f32, max_w: Option<f32>) -> Option<(Vec<LayoutGlyph>, f32)> {
        let (sx, sy) = (self.sx, self.sy);
        let fonts = &mut self.renderer.fonts;
        let metrics = Metrics::new(size * sy, size * 1.25 * sy);
        let mut buffer = Buffer::new(fonts, metrics);
        buffer.set_wrap(Wrap::None);
        if let Some(max_w) = max_w {
            buffer.set_ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)));
            buffer.set_size(Some(max_w * sx), None);
        }
        buffer.set_text(text, &Attrs::new().family(Family::Name(FAMILY)), Shaping::Advanced, None);
        buffer.shape_until_scroll(fonts, false);
        let run = buffer.layout_runs().next()?;
        Some((run.glyphs.to_vec(), run.line_w))
    }

    /// Glyphs outside the physical columns `clip` are cut off.
    fn draw_glyphs(&mut self, glyphs: &[LayoutGlyph], origin: (f32, f32), clip: (i32, i32), color: Rgba) {
        let Renderer { fonts, glyphs: cache } = &mut *self.renderer;
        for glyph in glyphs {
            let pg = glyph.physical(origin, 1.0);
            let Some(image) = cache.get_image(fonts, pg.cache_key) else { continue };
            let at = (pg.x + image.placement.left, pg.y - image.placement.top);
            let size = (image.placement.width, image.placement.height);
            let data = &image.data;
            match image.content {
                SwashContent::Mask => blit(&mut self.pixmap, at, size, clip, |i| {
                    let cov = data[i] as u32;
                    let a = mul255(cov, color.a as u32);
                    (cov != 0)
                        .then(|| [mul255(color.r as u32, a), mul255(color.g as u32, a), mul255(color.b as u32, a), a])
                }),
                // Colour glyphs come as straight-alpha RGBA.
                SwashContent::Color => blit(&mut self.pixmap, at, size, clip, |i| {
                    let s = &data[i * 4..][..4];
                    let a = mul255(s[3] as u32, color.a as u32);
                    Some([mul255(s[0] as u32, a), mul255(s[1] as u32, a), mul255(s[2] as u32, a), a])
                }),
                SwashContent::SubpixelMask => {}
            }
        }
    }
}

/// How far the waveform swells in silence, out of its full height.
const QUIET_SWELL: f32 = 0.3;

/// Room noise (under about −52 dBFS) stays at the quiet swell; speech fills the rest.
fn loudness(level: f32) -> f32 {
    let t = ((level - 0.13) / 0.6).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn format_countdown(seconds: u32) -> String {
    if seconds >= 60 { format!("{}:{:02} left", seconds / 60, seconds % 60) } else { format!("{seconds} s left") }
}

fn paint(color: Rgba) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    paint.anti_alias = true;
    paint
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    // Cubic approximation of a quarter circle.
    let k = r * 0.552_284_8;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn polar(cx: f32, cy: f32, r: f32, deg: f32) -> (f32, f32) {
    let a = deg.to_radians();
    (cx + r * a.cos(), cy + r * a.sin())
}

/// `from` and `to` are degrees, clockwise on screen.
fn arc(pb: &mut PathBuilder, cx: f32, cy: f32, r: f32, from: f32, to: f32) {
    let (x, y) = polar(cx, cy, r, from);
    pb.move_to(x, y);
    let segments = ((to - from).abs() / 90.0).ceil().max(1.0) as usize;
    let step = (to - from) / segments as f32;
    for i in 0..segments {
        let a0 = (from + step * i as f32).to_radians();
        let a1 = (from + step * (i + 1) as f32).to_radians();
        let k = 4.0 / 3.0 * ((a1 - a0) / 4.0).tan() * r;
        let (x0, y0) = (cx + r * a0.cos(), cy + r * a0.sin());
        let (x3, y3) = (cx + r * a1.cos(), cy + r * a1.sin());
        pb.cubic_to(x0 - k * a0.sin(), y0 + k * a0.cos(), x3 + k * a1.sin(), y3 - k * a1.cos(), x3, y3);
    }
}

fn mul255(a: u32, b: u32) -> u32 {
    (a * b + 127) / 255
}

/// `src(i)` is the premultiplied colour of the image's `i`th pixel, if it has one.
fn blit(
    pixmap: &mut Pixmap,
    (x, y): (i32, i32),
    (w, h): (u32, u32),
    clip: (i32, i32),
    src: impl Fn(usize) -> Option<[u32; 4]>,
) {
    let (pw, ph) = (pixmap.width() as i32, pixmap.height() as i32);
    let columns = clip.0.max(0)..clip.1.min(pw);
    let data = pixmap.data_mut();
    for j in 0..h as i32 {
        let py = y + j;
        if !(0..ph).contains(&py) {
            continue;
        }
        for i in 0..w as i32 {
            let px = x + i;
            if !columns.contains(&px) {
                continue;
            }
            if let Some(src) = src((j * w as i32 + i) as usize) {
                blend(&mut data[((py * pw + px) * 4) as usize..][..4], src);
            }
        }
    }
}

fn blend(dst: &mut [u8], src: [u32; 4]) {
    let inv = 255 - src[3];
    for c in 0..4 {
        dst[c] = (src[c] + mul255(dst[c] as u32, inv)).min(255) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_reads_naturally() {
        assert_eq!(format_countdown(28), "28 s left");
        assert_eq!(format_countdown(65), "1:05 left");
    }

    #[test]
    fn outside_the_pill_stays_transparent() {
        let mut r = Renderer::new();
        let p = r.render(&OverlayState::Done, Wave::default(), &Style::default(), 240, 44, 1.0);
        assert_eq!(p.pixel(0, 0).unwrap().alpha(), 0);
        assert_eq!(p.pixel(239, 43).unwrap().alpha(), 0);
        assert!(p.pixel(120, 22).unwrap().alpha() > 200);
    }
}
