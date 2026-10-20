//! Software rendering of a popup into a premultiplied RGBA pixmap.

use std::f32::consts::{FRAC_PI_2, TAU};
use std::time::Instant;

use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::GlyphMetrics;
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{
    BlendMode, Color, FillRule, FilterQuality, IntRect, LineCap, LineJoin, Paint, Path as SkPath,
    PathBuilder, Pattern, Pixmap, PixmapPaint, Rect, SpreadMode, Stroke, Transform,
};

use tokio::task::JoinHandle;
use tracing::{Span, debug, instrument, warn};

use super::icon::ICON_F;
use super::motion::{Frame, Mark};
use crate::text::wrap;

/// Popup width in pixels.
pub(crate) const WIDTH: u32 = 360;
const FAMILIES: [&str; 4] = ["Noto Sans", "DejaVu Sans", "Cantarell", "Liberation Sans"];

const WIDTH_F: f32 = 360.0;
const PADDING: f32 = 14.0;
const ICON_GAP: f32 = 12.0;
const RADIUS: f32 = 8.0;
const TITLE_PX: f32 = 17.0;
const BODY_PX: f32 = 14.0;
const TITLE_BODY_GAP: f32 = 4.0;
const MAX_TITLE_LINES: usize = 2;
const MAX_BODY_LINES: usize = 6;
/// Upper bound of the popup width and height in device pixels.
const MAX_PIXELS: f32 = 4096.0;

const BACKGROUND: [u8; 4] = [32, 33, 38, 240];
const BORDER: [u8; 4] = [210, 212, 220, 255];
const TITLE_COLOR: [u8; 4] = [245, 245, 248, 255];
const BODY_COLOR: [u8; 4] = [205, 207, 214, 255];
/// Opacity of the application icon under the state overlay.
const ICON_OPACITY: f32 = 0.4;
/// Spinner arc of a waiting request.
const SPINNER_COLOR: [u8; 4] = [245, 245, 248, 220];
/// Full circle the spinner arc runs on.
const SPINNER_TRACK: [u8; 4] = [245, 245, 248, 56];
// Discs behind the outcome glyphs are semi-transparent over the dimmed
// application icon.
/// Disc of the checkmark of a touch.
const TOUCHED_DISC: [u8; 4] = [38, 140, 60, 170];
/// Disc of the cross of a cancel or failure.
const FAILED_DISC: [u8; 4] = [218, 54, 51, 170];
/// Disc of the clock of a timeout.
const TIMED_OUT_DISC: [u8; 4] = [210, 153, 34, 170];
const LIGHT_GLYPH: [u8; 4] = [255, 255, 255, 255];
const DARK_GLYPH: [u8; 4] = [32, 33, 38, 255];
/// Radius of the spinner circle, in logical pixels.
const SPINNER_RADIUS: f32 = 15.0;
const SPINNER_WIDTH: f32 = 4.0;
/// Length of the spinner arc, in turns.
const SPINNER_SWEEP: f32 = 0.3;
/// Line segments approximating the spinner arc.
const ARC_STEPS: u16 = 24;
/// Radius of the disc behind an outcome glyph, in logical pixels.
const DISC_RADIUS: f32 = 18.0;
const GLYPH_WIDTH: f32 = 3.0;
/// Size a mark appearing in a transition starts at, relative to its full size.
const POP_FROM: f32 = 0.5;

/// Font file contents and the face index within it.
#[derive(Debug)]
pub(crate) struct Font {
    data: Vec<u8>,
    index: u32,
}

impl Font {
    /// Loads the first available preferred sans-serif face, else any system face.
    #[instrument(skip_all)]
    pub(crate) fn load_system() -> Result<Self, FontError> {
        let started = Instant::now();
        let mut db = fontdb::Database::new();
        // System fonts are memory-mapped. A same-user process truncating a mapped
        // font file can cause SIGBUS.
        db.load_system_fonts();
        let preferred = FAMILIES.into_iter().find(|name| {
            db.faces()
                .any(|face| face.families.iter().any(|(family, _)| family == name))
        });
        let id = match preferred {
            Some(name) => {
                db.set_sans_serif_family(name);
                db.query(&fontdb::Query {
                    families: &[fontdb::Family::SansSerif],
                    ..fontdb::Query::default()
                })
            }
            None => db.faces().next().map(|face| face.id),
        }
        .ok_or(FontError::NotFound)?;
        let font = db
            .with_face_data(id, |data, index| Self {
                data: data.to_vec(),
                index,
            })
            .ok_or(FontError::Unreadable)?;
        font.font_ref()?;
        debug!(
            family = preferred.unwrap_or("first available"),
            faces = db.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "loaded popup font"
        );
        Ok(font)
    }

    fn font_ref(&self) -> Result<FontRef<'_>, FontError> {
        Ok(FontRef::from_index(&self.data, self.index)?)
    }
}

/// Why no popup font is available.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FontError {
    #[error("no system font found")]
    NotFound,
    #[error("font file could not be read")]
    Unreadable,
    #[error("font file is malformed")]
    Malformed(#[from] skrifa::raw::ReadError),
    #[error("font loader task failed")]
    LoaderFailed,
}

/// The popup font, loading on the blocking pool from startup until the
/// first render waits for it.
pub(crate) enum FontState {
    Loading(JoinHandle<Result<Font, FontError>>),
    Ready(Option<Font>),
}

impl FontState {
    /// Starts loading the system font on the blocking pool.
    pub(crate) fn load() -> Self {
        let span = Span::current();
        Self::Loading(tokio::task::spawn_blocking(move || {
            span.in_scope(Font::load_system)
        }))
    }

    /// Waits for the font to load; a failure is logged once and leaves no font.
    pub(crate) async fn ready(&mut self) {
        let Self::Loading(loader) = self else {
            return;
        };
        let result = match loader.await {
            Ok(result) => result,
            Err(err) => {
                warn!(error = &err as &dyn std::error::Error, "font loader failed");
                Err(FontError::LoaderFailed)
            }
        };
        let font = match result {
            Ok(font) => Some(font),
            Err(err) => {
                warn!(
                    error = &err as &dyn std::error::Error,
                    "no usable font; popups show no text"
                );
                None
            }
        };
        *self = Self::Ready(font);
    }

    pub(crate) fn get(&self) -> Option<&Font> {
        match self {
            Self::Ready(font) => font.as_ref(),
            Self::Loading(_) => None,
        }
    }
}

/// One font at one pixel size.
struct Face<'a> {
    charmap: skrifa::charmap::Charmap<'a>,
    metrics: GlyphMetrics<'a>,
    outlines: skrifa::outline::OutlineGlyphCollection<'a>,
    size: Size,
    ascent: f32,
    line_height: f32,
}

impl<'a> Face<'a> {
    fn new(font: &FontRef<'a>, px: f32) -> Self {
        let size = Size::new(px);
        let line = font.metrics(size, LocationRef::default());
        let line_height = line.ascent - line.descent + line.leading;
        Self {
            charmap: font.charmap(),
            metrics: font.glyph_metrics(size, LocationRef::default()),
            outlines: font.outline_glyphs(),
            size,
            ascent: line.ascent,
            line_height: if line_height > 0.0 {
                line_height
            } else {
                px * 1.25
            },
        }
    }

    fn glyph(&self, ch: char) -> GlyphId {
        self.charmap.map(ch).unwrap_or(GlyphId::NOTDEF)
    }

    fn advance(&self, ch: char) -> f32 {
        self.metrics.advance_width(self.glyph(ch)).unwrap_or(0.0)
    }

    /// Fills `text` with its baseline origin at (`x`, `baseline`), mapped
    /// to pixels by `transform`.
    fn draw(
        &self,
        pixmap: &mut Pixmap,
        text: &str,
        (x, baseline): (f32, f32),
        color: [u8; 4],
        transform: Transform,
    ) {
        let mut pen = Pen {
            builder: PathBuilder::new(),
            x,
            y: baseline,
        };
        for ch in text.chars() {
            let glyph = self.glyph(ch);
            if let Some(outline) = self.outlines.get(glyph) {
                let settings = DrawSettings::unhinted(self.size, LocationRef::default());
                if let Err(err) = outline.draw(settings, &mut pen) {
                    debug!(
                        error = &err as &dyn std::error::Error,
                        glyph = glyph.to_u32(),
                        "glyph left out"
                    );
                }
            }
            pen.x += self.metrics.advance_width(glyph).unwrap_or(0.0);
        }
        if let Some(path) = pen.builder.finish() {
            pixmap.fill_path(&path, &paint(color), FillRule::Winding, transform, None);
        }
    }
}

/// Collects glyph outlines, flipping font y-up coordinates to pixmap y-down.
struct Pen {
    builder: PathBuilder,
    x: f32,
    y: f32,
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.builder.move_to(self.x + x, self.y - y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.builder.line_to(self.x + x, self.y - y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.builder
            .quad_to(self.x + cx0, self.y - cy0, self.x + x, self.y - y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.builder.cubic_to(
            self.x + cx0,
            self.y - cy0,
            self.x + cx1,
            self.y - cy1,
            self.x + x,
            self.y - y,
        );
    }

    fn close(&mut self) {
        self.builder.close();
    }
}

/// The static layer of a rendered popup: frame, text and dimmed icon.
#[derive(Clone)]
pub(crate) struct Rendered {
    /// Pixels at `scale`.
    pixmap: Pixmap,
    /// Height in logical pixels; the width is [`WIDTH`].
    height: u32,
    scale: f32,
    /// Top of the icon slot in logical pixels.
    slot_top: f32,
}

impl Rendered {
    /// Returns the device pixels the state overlay covers: the icon slot,
    /// rounded outward to whole pixels.
    fn slot(&self) -> Option<Rect> {
        let scale = self.scale;
        Rect::from_ltrb(
            (PADDING * scale).floor(),
            (self.slot_top * scale).floor(),
            ((PADDING + ICON_F) * scale).ceil(),
            ((self.slot_top + ICON_F) * scale).ceil(),
        )
    }

    /// Returns the centre of the icon slot in logical pixels.
    fn center(&self) -> (f32, f32) {
        (PADDING + ICON_F / 2.0, self.slot_top + ICON_F / 2.0)
    }
}

/// A rendered popup with its state overlay.
pub(crate) struct Canvas {
    layer: Rendered,
    /// `layer` with the overlay drawn over its icon slot.
    pixmap: Pixmap,
}

impl Canvas {
    pub(crate) fn new(layer: Rendered, frame: &Frame) -> Self {
        let pixmap = layer.pixmap.clone();
        let mut canvas = Self { layer, pixmap };
        canvas.draw(frame);
        canvas
    }

    /// Replaces the overlay with `frame`, copying the icon slot back from
    /// the static layer; the text is not laid out again.
    pub(crate) fn draw(&mut self, frame: &Frame) {
        if let Some(slot) = self.layer.slot() {
            let restore = Paint {
                shader: Pattern::new(
                    self.layer.pixmap.as_ref(),
                    SpreadMode::Pad,
                    FilterQuality::Nearest,
                    1.0,
                    Transform::identity(),
                ),
                blend_mode: BlendMode::Source,
                anti_alias: false,
                ..Paint::default()
            };
            self.pixmap
                .fill_rect(slot, &restore, Transform::identity(), None);
        }
        let scale = self.layer.scale;
        draw_overlay(
            &mut self.pixmap,
            frame,
            self.layer.center(),
            Transform::from_scale(scale, scale),
        );
    }

    /// Returns the pixels at the scale the popup was rendered at.
    pub(crate) fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }

    /// Returns the height in logical pixels; the width is [`WIDTH`].
    pub(crate) fn height(&self) -> u32 {
        self.layer.height
    }

    /// Returns the device pixels [`Canvas::draw`] changes.
    pub(crate) fn slot(&self) -> Option<IntRect> {
        self.layer.slot().and_then(|slot| slot.round_out())
    }
}

/// Renders a popup [`WIDTH`] logical pixels wide with the height its text
/// needs, at `scale` device pixels per logical pixel.
///
/// Without a font only the frame and icon are drawn. Without an icon the
/// icon slot stays empty for the state overlay.
pub(crate) fn render(
    font: Option<&Font>,
    title: &str,
    body: &str,
    icon: Option<&Pixmap>,
    scale: f32,
) -> Option<Rendered> {
    let root = Transform::from_scale(scale, scale);
    let text_x = PADDING + ICON_F + ICON_GAP;
    let text_width = WIDTH_F - text_x - PADDING;

    let font = font.and_then(usable_font);
    let faces = font
        .as_ref()
        .map(|font| (Face::new(font, TITLE_PX), Face::new(font, BODY_PX)));
    let (title_lines, body_lines) = match &faces {
        Some((title_face, body_face)) => (
            wrap(title, text_width, MAX_TITLE_LINES, |ch| {
                title_face.advance(ch)
            }),
            wrap(body, text_width, MAX_BODY_LINES, |ch| body_face.advance(ch)),
        ),
        None => (Vec::new(), Vec::new()),
    };
    let text_height = match &faces {
        Some((title_face, body_face)) => {
            title_lines
                .iter()
                .map(|_| title_face.line_height)
                .sum::<f32>()
                + TITLE_BODY_GAP
                + body_lines
                    .iter()
                    .map(|_| body_face.line_height)
                    .sum::<f32>()
        }
        None => 0.0,
    };
    // A whole logical height keeps an integer scale's buffer an exact multiple.
    let height = (PADDING * 2.0 + text_height.max(ICON_F)).ceil();
    let mut pixmap = Pixmap::new(pixels(WIDTH_F * scale), pixels(height * scale))?;

    let frame = rounded_rect(0.5, 0.5, WIDTH_F - 1.0, height - 1.0, RADIUS)?;
    pixmap.fill_path(&frame, &paint(BACKGROUND), FillRule::Winding, root, None);
    pixmap.stroke_path(
        &frame,
        &paint(BORDER),
        &Stroke {
            width: 1.0,
            ..Stroke::default()
        },
        root,
        None,
    );

    let slot_top = PADDING + (height - PADDING * 2.0 - ICON_F) / 2.0;
    if let Some(icon) = icon {
        // The icon occupies ICON_F logical pixels. A device-scale icon is drawn
        // at one source pixel per device pixel.
        let fit = ICON_F / side(icon.width());
        pixmap.draw_pixmap(
            0,
            0,
            icon.as_ref(),
            &PixmapPaint {
                opacity: ICON_OPACITY,
                quality: FilterQuality::Bilinear,
                ..PixmapPaint::default()
            },
            root.pre_translate(PADDING, slot_top).pre_scale(fit, fit),
            None,
        );
    }

    if let Some((title_face, body_face)) = &faces {
        let mut top = PADDING + (height - PADDING * 2.0 - text_height) / 2.0;
        for line in &title_lines {
            let origin = (text_x, top + title_face.ascent);
            title_face.draw(&mut pixmap, line, origin, TITLE_COLOR, root);
            top += title_face.line_height;
        }
        top += TITLE_BODY_GAP;
        for line in &body_lines {
            let origin = (text_x, top + body_face.ascent);
            body_face.draw(&mut pixmap, line, origin, BODY_COLOR, root);
            top += body_face.line_height;
        }
    }
    Some(Rendered {
        pixmap,
        height: pixels(height),
        scale,
        slot_top,
    })
}

/// Copies premultiplied RGBA pixels into a `wl_shm` `Argb8888` buffer,
/// which stores each pixel as B, G, R, A bytes.
pub(crate) fn copy_to_argb8888(src: &[u8], dst: &mut [u8]) {
    let (dst, _) = dst.as_chunks_mut::<4>();
    let (src, _) = src.as_chunks::<4>();
    for ([b, g, r, a], [sr, sg, sb, sa]) in dst.iter_mut().zip(src) {
        *b = *sb;
        *g = *sg;
        *r = *sr;
        *a = *sa;
    }
}

fn usable_font(font: &Font) -> Option<FontRef<'_>> {
    match font.font_ref() {
        Ok(font) => Some(font),
        Err(err) => {
            warn!(
                error = &err as &dyn std::error::Error,
                "popup font unusable"
            );
            None
        }
    }
}

fn rounded_rect(left: f32, top: f32, width: f32, height: f32, radius: f32) -> Option<SkPath> {
    // Control point distance of a cubic Bézier approximating a quarter circle.
    let handle = 0.552_284_8 * radius;
    let (right, bottom) = (left + width, top + height);
    let mut pb = PathBuilder::new();
    pb.move_to(left + radius, top);
    pb.line_to(right - radius, top);
    pb.cubic_to(
        right - radius + handle,
        top,
        right,
        top + radius - handle,
        right,
        top + radius,
    );
    pb.line_to(right, bottom - radius);
    pb.cubic_to(
        right,
        bottom - radius + handle,
        right - radius + handle,
        bottom,
        right - radius,
        bottom,
    );
    pb.line_to(left + radius, bottom);
    pb.cubic_to(
        left + radius - handle,
        bottom,
        left,
        bottom - radius + handle,
        left,
        bottom - radius,
    );
    pb.line_to(left, top + radius);
    pb.cubic_to(
        left,
        top + radius - handle,
        left + radius - handle,
        top,
        left + radius,
        top,
    );
    pb.close();
    pb.finish()
}

fn paint(color: [u8; 4]) -> Paint<'static> {
    faded(color, 1.0)
}

/// Returns an anti-aliased paint of `color` with its alpha multiplied by `opacity`.
fn faded(color: [u8; 4], opacity: f32) -> Paint<'static> {
    let [r, g, b, a] = color;
    let mut color = Color::from_rgba8(r, g, b, a);
    color.apply_opacity(opacity);
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

/// Draws `frame` centred on `(x, y)` in logical pixels, mapped to pixels by
/// `root`: the previous mark fades out while the current one fades in,
/// growing from [`POP_FROM`] of its size.
fn draw_overlay(pixmap: &mut Pixmap, frame: &Frame, (x, y): (f32, f32), root: Transform) {
    let at = root.pre_translate(x, y);
    if let Some(previous) = frame.previous {
        draw_mark(pixmap, previous, frame.spin, 1.0 - frame.progress, at);
    }
    let size = POP_FROM + (1.0 - POP_FROM) * frame.progress;
    draw_mark(
        pixmap,
        frame.mark,
        frame.spin,
        frame.progress,
        at.pre_scale(size, size),
    );
}

/// Draws `mark` centred on the origin of `at` at `opacity`; the spinner arc
/// starts at the top, turned clockwise by `spin` turns.
fn draw_mark(pixmap: &mut Pixmap, mark: Mark, spin: f32, opacity: f32, at: Transform) {
    if opacity <= 0.0 {
        return;
    }
    let (disc, color, glyph) = match mark {
        Mark::Spinner => {
            let start = spin * TAU - FRAC_PI_2;
            let track = PathBuilder::from_circle(0.0, 0.0, SPINNER_RADIUS);
            let arc = arc(SPINNER_RADIUS, start, SPINNER_SWEEP * TAU);
            for (path, color) in [(track, SPINNER_TRACK), (arc, SPINNER_COLOR)] {
                if let Some(path) = path {
                    stroke(pixmap, &path, color, opacity, SPINNER_WIDTH, at);
                }
            }
            return;
        }
        Mark::Check => (TOUCHED_DISC, LIGHT_GLYPH, check()),
        Mark::Cross => (FAILED_DISC, LIGHT_GLYPH, cross()),
        Mark::Clock => (TIMED_OUT_DISC, DARK_GLYPH, clock()),
    };
    if let Some(circle) = PathBuilder::from_circle(0.0, 0.0, DISC_RADIUS) {
        pixmap.fill_path(&circle, &faded(disc, opacity), FillRule::Winding, at, None);
    }
    if let Some(glyph) = glyph {
        stroke(pixmap, &glyph, color, opacity, GLYPH_WIDTH, at);
    }
}

fn stroke(
    pixmap: &mut Pixmap,
    path: &SkPath,
    color: [u8; 4],
    opacity: f32,
    width: f32,
    at: Transform,
) {
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    pixmap.stroke_path(path, &faded(color, opacity), &stroke, at, None);
}

/// Returns an arc of `radius` around the origin from angle `start`
/// clockwise over `sweep`, in radians.
fn arc(radius: f32, start: f32, sweep: f32) -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    pb.move_to(radius * start.cos(), radius * start.sin());
    for step in 1..=ARC_STEPS {
        let angle = start + sweep * f32::from(step) / f32::from(ARC_STEPS);
        pb.line_to(radius * angle.cos(), radius * angle.sin());
    }
    pb.finish()
}

fn check() -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    pb.move_to(-7.0, 0.5);
    pb.line_to(-2.5, 5.0);
    pb.line_to(7.0, -5.0);
    pb.finish()
}

fn cross() -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    pb.move_to(-6.0, -6.0);
    pb.line_to(6.0, 6.0);
    pb.move_to(6.0, -6.0);
    pb.line_to(-6.0, 6.0);
    pb.finish()
}

/// A clock face with its hands at three o'clock.
fn clock() -> Option<SkPath> {
    let mut pb = PathBuilder::new();
    pb.push_circle(0.0, 0.0, 10.0);
    pb.move_to(0.0, -5.5);
    pb.line_to(0.0, 0.0);
    pb.line_to(4.5, 0.0);
    pb.finish()
}

/// Converts an icon's side, at most four times [`ICON_F`], to `f32` exactly.
#[expect(
    clippy::cast_precision_loss,
    reason = "icon sides are small enough to be exact"
)]
fn side(pixels: u32) -> f32 {
    pixels.max(1) as f32
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to 1..=MAX_PIXELS first"
)]
fn pixels(length: f32) -> u32 {
    length.ceil().clamp(1.0, MAX_PIXELS) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("the popup was not rendered")]
    struct NotRendered;

    #[test]
    fn opaque_red_becomes_bgra() {
        let mut dst = [0_u8; 8];
        copy_to_argb8888(&[255, 0, 0, 255, 1, 2, 3, 4], &mut dst);
        assert_eq!(dst, [0, 0, 255, 255, 3, 2, 1, 4]);
    }

    const MARKS: [Mark; 4] = [Mark::Spinner, Mark::Check, Mark::Cross, Mark::Clock];

    fn canvas_at(scale: f32, frame: &Frame) -> Result<Canvas, NotRendered> {
        let rendered = render(None, "title", "body", None, scale).ok_or(NotRendered)?;
        Ok(Canvas::new(rendered, frame))
    }

    /// Returns the pixels of `pixmap` with the overlay's icon slot cleared.
    fn outside_slot(pixmap: &Pixmap, layer: &Rendered) -> Result<Vec<u8>, NotRendered> {
        let mut pixmap = pixmap.clone();
        let slot = layer.slot().ok_or(NotRendered)?;
        let clear = Paint {
            blend_mode: BlendMode::Clear,
            ..Paint::default()
        };
        pixmap.fill_rect(slot, &clear, Transform::identity(), None);
        Ok(pixmap.data().to_vec())
    }

    #[test]
    fn every_scale_keeps_the_logical_size() -> Result<(), NotRendered> {
        let logical = render(None, "title", "body", None, 1.0).ok_or(NotRendered)?;
        for scale in [0.75_f32, 0.8, 1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            let scaled = render(None, "title", "body", None, scale).ok_or(NotRendered)?;
            assert_eq!(scaled.height, logical.height, "scale {scale}");
            assert_eq!(
                scaled.pixmap.height(),
                pixels(side(logical.height) * scale),
                "scale {scale}"
            );
            for mark in MARKS {
                let canvas = Canvas::new(scaled.clone(), &Frame::settled(mark));
                assert_eq!(canvas.height(), logical.height, "scale {scale}");
                assert_eq!(
                    (canvas.pixmap().width(), canvas.pixmap().height()),
                    (scaled.pixmap.width(), scaled.pixmap.height()),
                    "scale {scale}"
                );
                let slot = canvas.slot().ok_or(NotRendered)?;
                assert!(
                    slot.right().unsigned_abs() <= canvas.pixmap().width()
                        && slot.bottom().unsigned_abs() <= canvas.pixmap().height(),
                    "scale {scale}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn each_state_draws_its_own_overlay_in_the_icon_slot() -> Result<(), NotRendered> {
        for scale in [1.0_f32, 1.5, 2.0] {
            let rendered = render(None, "title", "body", None, scale).ok_or(NotRendered)?;
            let base = outside_slot(&rendered.pixmap, &rendered)?;
            let mut drawn: Vec<Vec<u8>> = Vec::new();
            for mark in MARKS {
                let canvas = Canvas::new(rendered.clone(), &Frame::settled(mark));
                assert_ne!(
                    canvas.pixmap().data(),
                    rendered.pixmap.data(),
                    "{mark:?} at {scale}"
                );
                assert_eq!(
                    outside_slot(canvas.pixmap(), &rendered)?,
                    base,
                    "{mark:?} at {scale}"
                );
                assert!(
                    !drawn.iter().any(|other| other == canvas.pixmap().data()),
                    "{mark:?} at {scale}"
                );
                drawn.push(canvas.pixmap().data().to_vec());
            }
        }
        Ok(())
    }

    #[test]
    fn redrawing_replaces_the_overlay() -> Result<(), NotRendered> {
        let spinner = Frame::settled(Mark::Spinner);
        let mut canvas = canvas_at(1.5, &Frame::settled(Mark::Check))?;
        canvas.draw(&spinner);
        assert_eq!(
            canvas.pixmap().data(),
            canvas_at(1.5, &spinner)?.pixmap().data()
        );
        Ok(())
    }

    #[test]
    fn animation_frames_differ_from_the_settled_frame() -> Result<(), NotRendered> {
        let settled = canvas_at(1.0, &Frame::settled(Mark::Spinner))?;
        let turned = canvas_at(
            1.0,
            &Frame {
                spin: 0.25,
                ..Frame::settled(Mark::Spinner)
            },
        )?;
        assert_ne!(settled.pixmap().data(), turned.pixmap().data());
        let popping = canvas_at(
            1.0,
            &Frame {
                mark: Mark::Check,
                previous: Some(Mark::Spinner),
                progress: 0.5,
                spin: 0.0,
            },
        )?;
        let checked = canvas_at(1.0, &Frame::settled(Mark::Check))?;
        assert_ne!(popping.pixmap().data(), checked.pixmap().data());
        Ok(())
    }

    #[test]
    fn icon_is_dimmed_under_the_overlay() -> Result<(), NotRendered> {
        let mut icon = Pixmap::new(48, 48).ok_or(NotRendered)?;
        icon.fill(Color::WHITE);
        let rendered = render(None, "title", "body", Some(&icon), 1.0).ok_or(NotRendered)?;
        let canvas = Canvas::new(rendered, &Frame::settled(Mark::Spinner));
        // A corner of the icon slot, outside the spinner.
        let corner = canvas.pixmap().pixel(16, 16).ok_or(NotRendered)?;
        let [background, ..] = BACKGROUND;
        assert!(
            corner.red() > background && corner.red() < 160,
            "{corner:?}"
        );
        Ok(())
    }

    /// Returns the WCAG contrast ratio of two opaque sRGB colours.
    fn contrast(a: [u8; 4], b: [u8; 4]) -> f64 {
        let luminance = |[r, g, b, _]: [u8; 4]| {
            let linear = |channel: u8| {
                let value = f64::from(channel) / 255.0;
                if value <= 0.040_45 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
        };
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn outcome_colours_have_non_text_contrast() {
        for (disc, glyph) in [
            (TOUCHED_DISC, LIGHT_GLYPH),
            (FAILED_DISC, LIGHT_GLYPH),
            (TIMED_OUT_DISC, DARK_GLYPH),
        ] {
            assert!(contrast(disc, BACKGROUND) >= 3.0, "{disc:?}");
            assert!(contrast(disc, glyph) >= 3.0, "{disc:?}");
        }
    }

    #[test]
    fn renders_without_font_or_icon() {
        let size = render(None, "title", "body", None, 1.0)
            .map(|rendered| (rendered.pixmap.width(), rendered.pixmap.height()));
        assert_eq!(size, Some((WIDTH, pixels(PADDING * 2.0 + ICON_F))));
        let doubled = render(None, "title", "body", None, 2.0)
            .map(|rendered| (rendered.pixmap.width(), rendered.pixmap.height()));
        assert_eq!(
            doubled,
            Some((WIDTH * 2, pixels((PADDING * 2.0 + ICON_F) * 2.0)))
        );
    }
}
