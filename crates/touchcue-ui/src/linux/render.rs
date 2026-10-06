//! Software rendering of a popup into a premultiplied RGBA pixmap.

use std::time::Instant;

use skrifa::instance::{LocationRef, Size};
use skrifa::metrics::GlyphMetrics;
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{
    Color, FillRule, FilterQuality, Paint, Path as SkPath, PathBuilder, Pixmap, PixmapPaint,
    Stroke, Transform,
};

use tokio::task::JoinHandle;
use tracing::{Span, debug, instrument, warn};

use super::icon::ICON_F;
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
        // fontdb memory-maps every system font (about 35 ms instead of about
        // 7 s of reads); a same-user process truncating a mapped file can raise SIGBUS.
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

/// Renders a popup [`WIDTH`] logical pixels wide with the height its text
/// needs, at `scale` device pixels per logical pixel.
///
/// Without a font only the frame and icon are drawn.
pub(crate) fn render(
    font: Option<&Font>,
    title: &str,
    body: &str,
    icon: Option<&Pixmap>,
    scale: f32,
) -> Option<Pixmap> {
    let root = Transform::from_scale(scale, scale);
    let text_x = if icon.is_some() {
        PADDING + ICON_F + ICON_GAP
    } else {
        PADDING
    };
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
    let height = PADDING * 2.0 + text_height.max(ICON_F);
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

    if let Some(icon) = icon {
        let top = PADDING + (height - PADDING * 2.0 - ICON_F) / 2.0;
        pixmap.draw_pixmap(
            0,
            0,
            icon.as_ref(),
            &PixmapPaint {
                quality: FilterQuality::Bilinear,
                ..PixmapPaint::default()
            },
            root.pre_translate(PADDING, top),
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
    Some(pixmap)
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
    let [r, g, b, a] = color;
    let mut paint = Paint::default();
    paint.set_color(Color::from_rgba8(r, g, b, a));
    paint.anti_alias = true;
    paint
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

    #[test]
    fn opaque_red_becomes_bgra() {
        let mut dst = [0_u8; 8];
        copy_to_argb8888(&[255, 0, 0, 255, 1, 2, 3, 4], &mut dst);
        assert_eq!(dst, [0, 0, 255, 255, 3, 2, 1, 4]);
    }

    #[test]
    fn renders_without_font_or_icon() {
        let size = render(None, "title", "body", None, 1.0).map(|p| (p.width(), p.height()));
        assert_eq!(size, Some((WIDTH, pixels(PADDING * 2.0 + ICON_F))));
        let doubled = render(None, "title", "body", None, 2.0).map(|p| (p.width(), p.height()));
        assert_eq!(
            doubled,
            Some((WIDTH * 2, pixels((PADDING * 2.0 + ICON_F) * 2.0)))
        );
    }
}
