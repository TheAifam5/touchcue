//! Popup stacks rendered without a display connection, at an explicit
//! animation time.

use std::num::TryFromIntError;
use std::time::{Duration, Instant};

use tiny_skia::{Color, Pixmap, PixmapPaint, Transform};
use touchcue_core::{RequestId, RequestState};

use super::icon::{IconError, icon_pixels, load_icon};
use super::motion::Animations;
use super::placement::stack;
use super::render::{self, Canvas, Font, FontError};
use super::wayland::SPACING;
use crate::Prompt;

/// Device pixels per logical pixel.
const SCALE: u32 = 2;
const SCALE_F: f32 = 2.0;
const SCALE_I: i32 = 2;

/// Why an offscreen popup stack cannot be rendered.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct OffscreenError(Failure);

#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error("failed to load the popup font")]
    Font(#[from] FontError),
    #[error("failed to load a popup icon")]
    Icon(#[from] IconError),
    #[error("popup size is out of range")]
    Size(#[from] TryFromIntError),
    #[error("popup has no drawable area")]
    Empty,
}

/// Popups stacked as on a Wayland output, rendered at scale 2 with
/// animated state overlays.
pub struct OffscreenStack {
    start: Instant,
    animations: Animations,
    canvases: Vec<(RequestId, Canvas)>,
    offsets: Vec<i32>,
    pixmap: Pixmap,
}

impl OffscreenStack {
    /// Renders `prompts` top to bottom at animation time zero, with the
    /// system popup font.
    ///
    /// # Errors
    /// Returns an error if the font or an icon fails to load, or a popup
    /// has no drawable size.
    pub fn new(prompts: &[Prompt]) -> Result<Self, OffscreenError> {
        Self::load(prompts).map_err(OffscreenError)
    }

    fn load(prompts: &[Prompt]) -> Result<Self, Failure> {
        let font = Font::load_system()?;
        let start = Instant::now();
        let mut animations = Animations::new(true);
        let mut canvases = Vec::with_capacity(prompts.len());
        let mut heights = Vec::with_capacity(prompts.len());
        for prompt in prompts {
            let icon = prompt
                .icon
                .as_deref()
                .map(|path| load_icon(path, icon_pixels(SCALE_F)))
                .transpose()?;
            let layer = render::render(
                Some(&font),
                &prompt.title,
                &prompt.body,
                icon.as_ref(),
                SCALE_F,
            )
            .ok_or(Failure::Empty)?;
            let canvas = Canvas::new(layer, &animations.show(prompt.id, prompt.state, start));
            heights.push(i32::try_from(canvas.height())?);
            canvases.push((prompt.id, canvas));
        }
        let (offsets, height) = stack(&heights, SPACING);
        let pixmap = Pixmap::new(render::WIDTH * SCALE, u32::try_from(height)? * SCALE)
            .ok_or(Failure::Empty)?;
        Ok(Self {
            start,
            animations,
            canvases,
            offsets,
            pixmap,
        })
    }

    /// Moves popup `id` to `state` `elapsed` after time zero; changes are
    /// applied in time order.
    pub fn set_state(&mut self, id: RequestId, state: RequestState, elapsed: Duration) {
        self.animations.show(id, state, self.start + elapsed);
    }

    /// Returns the width and height of a frame in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    /// Returns the frame `elapsed` after time zero as unpremultiplied RGBA
    /// rows; pixels outside the popups are transparent.
    pub fn frame(&mut self, elapsed: Duration) -> Vec<u8> {
        self.pixmap.fill(Color::TRANSPARENT);
        for ((id, canvas), offset) in self.canvases.iter_mut().zip(&self.offsets) {
            if let Some(frame) = self.animations.frame(*id, self.start + elapsed) {
                canvas.draw(&frame);
            }
            self.pixmap.draw_pixmap(
                0,
                offset.saturating_mul(SCALE_I),
                canvas.pixmap().as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
        self.pixmap
            .pixels()
            .iter()
            .flat_map(|pixel| {
                let color = pixel.demultiply();
                [color.red(), color.green(), color.blue(), color.alpha()]
            })
            .collect()
    }
}
