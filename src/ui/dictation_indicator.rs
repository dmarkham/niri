//! A small paperclip that appears while dictation is listening.
//!
//! Without this there is no way to tell the microphone is open: in an
//! application that speaks text-input you at least see preedit appear, but
//! everywhere else nothing happens at all until a whole utterance settles.
//!
//! The clip is drawn as cairo paths rather than an emoji glyph so it does not
//! depend on which fonts happen to be installed, and so it stays crisp at any
//! output scale.

use std::cell::RefCell;
use std::collections::HashMap;
use std::f64::consts::PI;
use std::rc::Rc;

use niri_config::Config;
use ordered_float::NotNan;
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::{EllipsizeMode, FontDescription};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::output::Output;
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Point, Transform};

use crate::animation::{Animation, Clock};
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{output_size, to_physical_precise_round};

/// Size of the badge, and how far it sits from the corner of the output.
const BADGE: i32 = 44;
const MARGIN: i32 = 24;
const CORNER: f64 = 12.;

/// The paperclip is laid out in its own coordinate space and scaled to fit,
/// which keeps the path readable and the badge size a single knob.
const CLIP_W: f64 = 14.;
const CLIP_H: f64 = 58.;
/// How much of the badge the clip fills.
const CLIP_FILL: f64 = 0.62;

/// Live transcript shown beside the clip.
const FONT: &str = "sans 14px";
const TEXT_PAD: f64 = 10.;
/// Cap on the transcript strip. Longer previews keep their tail, since the
/// newest words are the ones being checked against what was just said.
const MAX_TEXT: f64 = 520.;

pub struct DictationIndicator {
    state: State,
    /// Words heard but not yet settled. Shown beside the clip so dictation is
    /// legible in applications that cannot display preedit themselves — which
    /// is most of them, Chromium included.
    preview: String,
    buffers: RefCell<HashMap<NotNan<f64>, Option<TextureBuffer<GlesTexture>>>>,
    clock: Clock,
    config: Rc<RefCell<Config>>,
}

enum State {
    Hidden,
    Showing(Animation),
    Shown,
    Hiding(Animation),
}

impl DictationIndicator {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            state: State::Hidden,
            preview: String::new(),
            buffers: RefCell::new(HashMap::new()),
            clock,
            config,
        }
    }

    fn animation(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.config_notification_open_close.0,
        )
    }

    pub fn show(&mut self) {
        let from = match &self.state {
            State::Shown | State::Showing(_) => return,
            // Catch a clip on its way out rather than restarting from nothing.
            State::Hiding(anim) => anim.value(),
            State::Hidden => 0.,
        };
        self.state = State::Showing(self.animation(from, 1.));
    }

    pub fn hide(&mut self) {
        let from = match &self.state {
            State::Hidden | State::Hiding(_) => return,
            State::Showing(anim) => anim.value(),
            State::Shown => 1.,
        };
        self.state = State::Hiding(self.animation(from, 0.));
    }

    /// Update the words shown beside the clip. Returns whether anything
    /// changed, so the caller only asks for a frame when one is warranted.
    pub fn set_preview(&mut self, text: &str) -> bool {
        if self.preview == text {
            return false;
        }
        text.clone_into(&mut self.preview);
        // The text is baked into the texture, so it has to be redrawn.
        self.buffers.borrow_mut().clear();
        true
    }

    pub fn advance_animations(&mut self) {
        match &self.state {
            State::Showing(anim) if anim.is_done() => self.state = State::Shown,
            State::Hiding(anim) if anim.is_done() => self.state = State::Hidden,
            _ => (),
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_))
    }

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
    ) -> Option<PrimaryGpuTextureRenderElement> {
        if matches!(self.state, State::Hidden) {
            return None;
        }

        let scale = output.current_scale().fractional_scale();
        let output_size = output_size(output);

        let preview = self.preview.clone();
        let mut buffers = self.buffers.borrow_mut();
        let buffer = buffers
            .entry(NotNan::new(scale).unwrap())
            .or_insert_with(move || render(renderer.as_gles_renderer(), scale, &preview).ok());
        let buffer = buffer.clone()?;

        // Bottom right, out of the way of the config notification at the top
        // and of whatever is being dictated into.
        let size = buffer.logical_size();
        let margin = f64::from(MARGIN);
        let x = output_size.w - size.w - margin;

        // Slide up out of the corner rather than appearing all at once.
        let progress = match &self.state {
            State::Hidden => return None,
            State::Showing(anim) | State::Hiding(anim) => anim.value(),
            State::Shown => 1.,
        };
        let travel = size.h + margin;
        let y = output_size.h - margin - size.h + travel * (1. - progress.clamp(0., 1.));

        let location = Point::from((x, y));
        let location = location.to_physical_precise_round(scale).to_logical(scale);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            progress.clamp(0., 1.) as f32,
            None,
            None,
            Kind::Unspecified,
        );
        Some(PrimaryGpuTextureRenderElement(elem))
    }
}

fn render(
    renderer: &mut GlesRenderer,
    scale: f64,
    preview: &str,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let _span = tracy_client::span!("dictation_indicator::render");

    let badge: f64 = f64::from(to_physical_precise_round::<i32>(scale, BADGE));

    // Measure first: the strip is only as wide as the words in it.
    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));

    let text_width = if preview.is_empty() {
        0.
    } else {
        let probe = ImageSurface::create(cairo::Format::ARgb32, 0, 0)?;
        let cr = cairo::Context::new(&probe)?;
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&font));
        layout.set_text(preview);
        let width = f64::from(layout.pixel_size().0);
        width.min(MAX_TEXT * scale) + TEXT_PAD * scale * 2.
    };

    let width = (badge + text_width).round() as i32;
    let height = badge.round() as i32;

    let surface = ImageSurface::create(cairo::Format::ARgb32, width, height)?;
    let cr = cairo::Context::new(&surface)?;
    draw_badge(&cr, f64::from(width), badge, scale)?;

    if !preview.is_empty() {
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&font));
        layout.set_text(preview);
        // Keep the tail: the newest words are the ones being checked.
        layout.set_ellipsize(EllipsizeMode::Start);
        layout.set_width(
            (text_width - TEXT_PAD * scale * 2.).max(0.) as i32 * pangocairo::pango::SCALE,
        );

        let text_height = f64::from(layout.pixel_size().1);
        cr.move_to(badge, (badge - text_height) / 2.);
        cr.set_source_rgba(0.85, 0.87, 0.92, 0.95);
        pangocairo::functions::show_layout(&cr, &layout);
    }

    drop(cr);

    let data = surface.take_data().unwrap();
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (width, height),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;

    Ok(buffer)
}

/// Paint the pill and the clip. Separated from the texture upload so it can be
/// rendered and looked at without a GPU.
fn draw_badge(cr: &cairo::Context, width: f64, badge: f64, scale: f64) -> anyhow::Result<()> {
    let corner = CORNER * scale;

    // Rounded pill, dark and mostly opaque so it reads on any wallpaper.
    rounded_rect(cr, 0., 0., width, badge, corner);
    cr.set_source_rgba(0.09, 0.09, 0.11, 0.92);
    cr.fill_preserve()?;
    cr.set_source_rgba(1., 1., 1., 0.16);
    cr.set_line_width(scale.max(1.));
    cr.stroke()?;

    // The clip keeps its own square at the left, whatever the text does.
    let clip_scale = badge * CLIP_FILL / CLIP_H;
    let drawn_w = CLIP_W * clip_scale;
    let drawn_h = CLIP_H * clip_scale;
    cr.save()?;
    cr.translate((badge - drawn_w) / 2., (badge - drawn_h) / 2.);
    cr.scale(clip_scale, clip_scale);

    paperclip(cr);
    // Line width is in the scaled space, so express it as a fraction of the
    // clip rather than in pixels.
    cr.set_line_width(CLIP_W * 0.26);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_source_rgb(0.85, 0.87, 0.92);
    cr.stroke()?;
    cr.restore()?;

    Ok(())
}

/// The wire path of a gem paperclip, drawn in a 14x58 space with the origin at
/// its top left.
///
/// A paperclip is a spiral, so every bend turns the same way — that is the
/// sanity check on the arcs below. Cairo's y axis points down, so the
/// consistently-inward turn is the negative (counter-clockwise) direction.
fn paperclip(cr: &cairo::Context) {
    // Local coordinates, shifted so the drawn shape starts at (0, 0).
    const DX: f64 = -10.;
    const DY: f64 = -15.;
    let x = |v: f64| v + DX;
    let y = |v: f64| v + DY;

    // Down the long outer arm.
    cr.move_to(x(10.), y(20.));
    cr.line_to(x(10.), y(66.));
    // Round the bottom, left to right.
    cr.arc_negative(x(17.), y(66.), 7., PI, 0.);
    // Up the other outer arm.
    cr.line_to(x(24.), y(20.));
    // Over the top, right to left, tucking inside.
    cr.arc_negative(x(19.), y(20.), 5., 2. * PI, PI);
    // Down the inner arm.
    cr.line_to(x(14.), y(58.));
    // Small bend at the bottom.
    cr.arc_negative(x(17.), y(58.), 3., PI, 0.);
    // And back up to the free end.
    cr.line_to(x(20.), y(30.));
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.).min(h / 2.);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2., 0.);
    cr.arc(x + w - r, y + h - r, r, 0., PI / 2.);
    cr.arc(x + r, y + h - r, r, PI / 2., PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the badge large and write it out, so the shape can actually be
    /// looked at rather than assumed. A paperclip is a visual requirement.
    #[test]
    fn draws_a_paperclip() {
        let size = 220;
        let surface = ImageSurface::create(cairo::Format::ARgb32, size, size).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        draw_badge(&cr, f64::from(size), f64::from(size), 5.).unwrap();
        drop(cr);

        let stride = surface.stride() as usize;
        let data = surface.take_data().unwrap();

        // Cairo keeps ARGB32 as premultiplied BGRA on little-endian.
        let mut rgba = Vec::with_capacity((size * size * 4) as usize);
        for row in 0..size as usize {
            for col in 0..size as usize {
                let i = row * stride + col * 4;
                let (b, g, r, a) = (data[i], data[i + 1], data[i + 2], data[i + 3]);
                let un = |c: u8| {
                    if a == 0 {
                        0
                    } else {
                        (u32::from(c) * 255 / u32::from(a)).min(255) as u8
                    }
                };
                rgba.extend_from_slice(&[un(r), un(g), un(b), a]);
            }
        }

        // Only written when asked for, so the suite does not litter:
        //     PAPERCLIP_PNG=/tmp/paperclip.png cargo test draws_a_paperclip
        if let Ok(path) = std::env::var("PAPERCLIP_PNG") {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder =
                png::Encoder::new(std::io::BufWriter::new(file), size as u32, size as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&rgba)
                .unwrap();
        }

        // Something was actually drawn: an opaque badge, and clip-coloured
        // pixels inside it. A silently empty texture would otherwise look
        // exactly like dictation not being active.
        let centre = (size as usize / 2) * stride + (size as usize / 2) * 4;
        assert!(data[centre + 3] > 0, "badge should not be transparent");

        let light = rgba
            .chunks_exact(4)
            .filter(|px| px[3] > 0 && px[0] > 180 && px[1] > 180 && px[2] > 180)
            .count();
        assert!(
            light > 200,
            "expected a drawn clip, found {light} light pixels"
        );
    }

    /// The strip has to actually contain the words, or dictation is invisible
    /// in every application that cannot render preedit itself.
    #[test]
    fn draws_the_preview_beside_the_clip() {
        let badge = 44.;
        let text = "this is what the transcriber heard";

        let probe = ImageSurface::create(cairo::Format::ARgb32, 0, 0).unwrap();
        let cr = cairo::Context::new(&probe).unwrap();
        let layout = pangocairo::functions::create_layout(&cr);
        let mut font = FontDescription::from_string(FONT);
        font.set_absolute_size(font.size().into());
        layout.set_font_description(Some(&font));
        layout.set_text(text);
        let text_w = f64::from(layout.pixel_size().0);
        drop(cr);

        assert!(text_w > 0., "pango measured no text");

        let width = (badge + text_w + TEXT_PAD * 2.) as i32;
        let surface = ImageSurface::create(cairo::Format::ARgb32, width, badge as i32).unwrap();
        let cr = cairo::Context::new(&surface).unwrap();
        draw_badge(&cr, f64::from(width), badge, 1.).unwrap();
        let layout = pangocairo::functions::create_layout(&cr);
        layout.set_font_description(Some(&font));
        layout.set_text(text);
        cr.move_to(badge, 8.);
        cr.set_source_rgba(0.85, 0.87, 0.92, 0.95);
        pangocairo::functions::show_layout(&cr, &layout);
        drop(cr);

        let stride = surface.stride() as usize;
        let data = surface.take_data().unwrap();

        let mut rgba = Vec::new();
        for row in 0..badge as usize {
            for col in 0..width as usize {
                let i = row * stride + col * 4;
                let (b, g, r, a) = (data[i], data[i + 1], data[i + 2], data[i + 3]);
                let un = |c: u8| {
                    if a == 0 {
                        0
                    } else {
                        (u32::from(c) * 255 / u32::from(a)).min(255) as u8
                    }
                };
                rgba.extend_from_slice(&[un(r), un(g), un(b), a]);
            }
        }
        if let Ok(path) = std::env::var("PILL_PNG") {
            let file = std::fs::File::create(&path).unwrap();
            let mut enc =
                png::Encoder::new(std::io::BufWriter::new(file), width as u32, badge as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&rgba).unwrap();
        }

        // Light pixels to the right of the clip means text landed there.
        let in_text_area = rgba
            .chunks_exact(4)
            .enumerate()
            .filter(|(i, px)| {
                let col = i % width as usize;
                col > badge as usize && px[3] > 0 && px[0] > 180 && px[1] > 180
            })
            .count();
        assert!(
            in_text_area > 100,
            "no text drawn: {in_text_area} light pixels"
        );
    }
}
