//! Pixel-sized artwork and font coverage for the desktop renderer.

use egui::{Color32, ColorImage, Context, TextureHandle, TextureId, TextureOptions};

pub fn pixel_aligned_rect(ctx: &Context, rect: egui::Rect) -> egui::Rect {
    let ppp = ctx.pixels_per_point();
    // Round the size independently so a square stays square at 125% DPI and
    // its texture is never stretched by one pixel depending on its position.
    egui::Rect::from_min_size(
        egui::pos2((rect.min.x * ppp).round() / ppp, (rect.min.y * ppp).round() / ppp),
        egui::vec2((rect.width() * ppp).round() / ppp, (rect.height() * ppp).round() / ppp),
    )
}

pub struct ForgeIcon {
    mask: Vec<f32>,
    size: [usize; 2],
    small: Option<TextureHandle>,
    large: Option<TextureHandle>,
}

pub enum IconPlacement { ActivityBar, Watermark }

impl ForgeIcon {
    pub fn load() -> Option<Self> {
        let image = crate::img::png::decode(include_bytes!("../Forge.png")).ok()?;
        // The asset is white artwork on black. Treat brightness as coverage,
        // rather than retaining dark edge pixels after keying out the black.
        let mask = image.rgba.chunks_exact(4).map(|p| {
            (p[0] as f32 + p[1] as f32 + p[2] as f32) / (3.0 * 255.0)
                * (p[3] as f32 / 255.0)
        }).collect();
        Some(Self { mask, size: [image.width, image.height], small: None, large: None })
    }

    pub fn texture(&mut self, ctx: &Context, physical_size: usize, placement: IconPlacement) -> TextureId {
        let side = physical_size.clamp(1, self.size[0].min(self.size[1]));
        // Select by usage, not pixel size: a high-DPI activity icon can be
        // larger than a watermark in a narrow pane, and both must stay cached.
        let slot = match placement {
            IconPlacement::ActivityBar => &mut self.small,
            IconPlacement::Watermark => &mut self.large,
        };
        if slot.as_ref().is_none_or(|t| t.size() != [side, side]) {
            // A bilinear lookup only samples four source pixels. At 1280 -> 30
            // that skips nearly all the artwork, so integrate the footprint first.
            let image = downsample_mask(&self.mask, self.size, side);
            if let Some(texture) = slot {
                texture.set(image, TextureOptions::LINEAR);
            } else {
                *slot = Some(ctx.load_texture("forge_icon", image, TextureOptions::LINEAR));
            }
        }
        slot.as_ref().unwrap().id()
    }
}

fn downsample_mask(mask: &[f32], size: [usize; 2], side: usize) -> ColorImage {
    let sx = size[0] as f32 / side as f32;
    let sy = size[1] as f32 / side as f32;
    let mut pixels = Vec::with_capacity(side * side);
    for y in 0..side {
        let (top, bottom) = (y as f32 * sy, (y + 1) as f32 * sy);
        for x in 0..side {
            let (left, right) = (x as f32 * sx, (x + 1) as f32 * sx);
            let mut coverage = 0.0;
            for iy in top.floor() as usize..(bottom.ceil() as usize).min(size[1]) {
                let wy = (bottom.min((iy + 1) as f32) - top.max(iy as f32)).max(0.0);
                for ix in left.floor() as usize..(right.ceil() as usize).min(size[0]) {
                    let wx = (right.min((ix + 1) as f32) - left.max(ix as f32)).max(0.0);
                    coverage += mask[iy * size[0] + ix] * wx * wy;
                }
            }
            pixels.push(Color32::from_white_alpha((coverage / (sx * sy) * 255.0).round() as u8));
        }
    }
    ColorImage { size: [side, side], pixels }
}

/// Keep the texture update's offset and filtering when replacing a partial atlas.
#[cfg(windows)]
pub fn sharpen_font_delta(delta: &mut egui::epaint::ImageDelta) {
    if let egui::ImageData::Font(font) = &delta.image {
        // egui 0.29's 0.55 coverage exponent brightens faint glyph fringes.
        // A gentler boost keeps antialiasing while reducing the soft halo at 1x.
        delta.image = egui::ImageData::Color(std::sync::Arc::new(ColorImage {
            size: font.size,
            pixels: font.srgba_pixels(Some(0.85)).collect(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minification_retains_thin_features_between_bilinear_samples() {
        let mut mask = vec![0.0; 64];
        for row in 0..8 { mask[row * 8 + 1] = 1.0; }
        let image = downsample_mask(&mask, [8, 8], 1);
        assert_eq!(image.pixels[0].a(), 32);
        // Sampling only the four center pixels (the old path) misses the line.
        assert_eq!(mask[3 * 8 + 3] + mask[3 * 8 + 4] + mask[4 * 8 + 3] + mask[4 * 8 + 4], 0.0);
    }

    #[test]
    fn fractional_minification_preserves_solid_coverage() {
        let image = downsample_mask(&vec![1.0; 49], [7, 7], 3);
        assert!(image.pixels.iter().all(|p| *p == Color32::WHITE));
    }

    #[test]
    fn high_dpi_icon_and_small_watermark_keep_separate_textures() {
        let ctx = Context::default();
        let mut icon = ForgeIcon::load().unwrap();
        let activity = icon.texture(&ctx, 90, IconPlacement::ActivityBar);
        let watermark = icon.texture(&ctx, 60, IconPlacement::Watermark);
        assert_ne!(activity, watermark);
        assert_eq!(icon.small.as_ref().unwrap().size(), [90, 90]);
        assert_eq!(icon.large.as_ref().unwrap().size(), [60, 60]);
        assert_eq!(icon.texture(&ctx, 90, IconPlacement::ActivityBar), activity);
    }

    #[test]
    fn images_land_on_physical_pixels_at_fractional_display_scales() {
        let ctx = Context::default();
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            ctx.set_pixels_per_point(ppp);
            let _ = ctx.run(egui::RawInput::default(), |_| {});
            let rect = pixel_aligned_rect(&ctx, egui::Rect::from_min_size(
                egui::pos2(13.3, 19.7), egui::vec2(31.0, 31.0),
            ));
            for value in [rect.min.x, rect.min.y, rect.max.x, rect.max.y] {
                assert!((value * ppp - (value * ppp).round()).abs() < 0.0001);
            }
            assert!((rect.width() - rect.height()).abs() < 0.0001);
        }
    }

    #[cfg(windows)]
    #[test]
    fn font_adjustment_does_not_change_image_colors() {
        let image = ColorImage::new([1, 1], Color32::from_rgb(90, 140, 220));
        let mut delta = egui::epaint::ImageDelta::full(image.clone(), TextureOptions::LINEAR);
        sharpen_font_delta(&mut delta);
        let egui::ImageData::Color(after) = delta.image else { panic!("image changed type") };
        assert_eq!(after.pixels, image.pixels);
    }

    #[cfg(windows)]
    #[test]
    fn font_edges_are_crisper_without_changing_partial_atlas_position() {
        let mut font = egui::FontImage::new([3, 1]);
        font.pixels = vec![0.0, 0.1, 1.0];
        let old_edge = font.srgba_pixels(None).nth(1).unwrap().a();
        let mut delta = egui::epaint::ImageDelta::partial([17, 23], font, TextureOptions::LINEAR);
        sharpen_font_delta(&mut delta);
        assert_eq!(delta.pos, Some([17, 23]));
        let egui::ImageData::Color(image) = delta.image else { panic!("font was not converted") };
        assert_eq!(image.pixels[0], Color32::TRANSPARENT);
        assert_eq!(image.pixels[2], Color32::WHITE);
        assert!(image.pixels[1].a() < old_edge);
    }
}
