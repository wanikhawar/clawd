//! Screenshots and image attachments.

use crate::claude::protocol::Image;
use gtk4::gdk_pixbuf::{InterpType, Pixbuf, PixbufLoader};
use gtk4::prelude::*;
use gtk4::{gdk, glib};
use std::process::Command;

/// Longest edge sent to Claude. Larger images are downscaled by the API
/// anyway, so sending more only costs upload time and tokens.
const MAX_EDGE: i32 = 1568;
/// The API's limit is 5 MB of base64, i.e. ~3.75 MB of raw bytes.
const MAX_BYTES: usize = 3_700_000;
/// Pending-attachment chip above the prompt box (cropped to fill).
pub const THUMB: (i32, i32) = (72, 54);
/// Image in the transcript (scaled to fit).
pub const PREVIEW: (i32, i32) = (200, 150);

pub struct Attachment {
    pub image: Image,
    /// Size of the image as sent to Claude.
    pub size: (i32, i32),
    /// GTK shows pictures at their pixel size, so the thumbnails are made at
    /// exactly the size they're displayed.
    pub thumb: gdk::Texture,
    pub preview: gdk::Texture,
}

/// Decodes an image (PNG, JPEG, …), downscales it if needed and re-encodes it
/// for Claude.
pub fn prepare(bytes: &[u8]) -> Result<Attachment, String> {
    let loader = PixbufLoader::new();
    loader.write(bytes).map_err(|e| e.to_string())?;
    loader.close().map_err(|e| e.to_string())?;
    let mut pixbuf = loader.pixbuf().ok_or("not an image")?;

    let (w, h) = (pixbuf.width(), pixbuf.height());
    let longest = w.max(h);
    if longest > MAX_EDGE {
        let scale = MAX_EDGE as f64 / longest as f64;
        let (sw, sh) = ((w as f64 * scale).round() as i32, (h as f64 * scale).round() as i32);
        pixbuf = pixbuf.scale_simple(sw, sh, InterpType::Bilinear).ok_or("couldn't resize the image")?;
    }

    let (data, media_type) = encode(&pixbuf)?;
    let image = Image { media_type, data: glib::base64_encode(&data).to_string() };
    Ok(Attachment {
        image,
        size: (pixbuf.width(), pixbuf.height()),
        thumb: texture(&cover(&pixbuf, THUMB)?)?,
        preview: texture(&fit(&pixbuf, PREVIEW)?)?,
    })
}

/// Scales down to fit within `max`, keeping the aspect ratio.
fn fit(pixbuf: &Pixbuf, (mw, mh): (i32, i32)) -> Result<Pixbuf, String> {
    let (w, h) = (pixbuf.width() as f64, pixbuf.height() as f64);
    let scale = (mw as f64 / w).min(mh as f64 / h).min(1.0);
    let size = ((w * scale).round().max(1.0) as i32, (h * scale).round().max(1.0) as i32);
    pixbuf.scale_simple(size.0, size.1, InterpType::Hyper).ok_or_else(|| "couldn't resize the image".into())
}

/// Scales to fill `size` exactly, cropping the overflow around the center.
fn cover(pixbuf: &Pixbuf, (cw, ch): (i32, i32)) -> Result<Pixbuf, String> {
    let (w, h) = (pixbuf.width() as f64, pixbuf.height() as f64);
    let scale = (cw as f64 / w).max(ch as f64 / h);
    let (sw, sh) = ((w * scale).ceil().max(cw as f64) as i32, (h * scale).ceil().max(ch as f64) as i32);
    let scaled = pixbuf.scale_simple(sw, sh, InterpType::Hyper).ok_or("couldn't resize the image")?;
    Ok(scaled.new_subpixbuf((sw - cw) / 2, (sh - ch) / 2, cw, ch))
}

fn texture(pixbuf: &Pixbuf) -> Result<gdk::Texture, String> {
    let png = pixbuf.save_to_bufferv("png", &[]).map_err(|e| e.to_string())?;
    gdk::Texture::from_bytes(&glib::Bytes::from(&png)).map_err(|e| e.to_string())
}

/// PNG keeps screenshots' text crisp; JPEG is the fallback for photos that
/// would be too large as PNG.
fn encode(pixbuf: &Pixbuf) -> Result<(Vec<u8>, &'static str), String> {
    let png = pixbuf.save_to_bufferv("png", &[]).map_err(|e| e.to_string())?;
    if png.len() <= MAX_BYTES {
        return Ok((png, "image/png"));
    }
    let opaque = if pixbuf.has_alpha() { flatten(pixbuf) } else { pixbuf.clone() };
    let jpeg = opaque.save_to_bufferv("jpeg", &[("quality", "85")]).map_err(|e| e.to_string())?;
    if jpeg.len() <= MAX_BYTES {
        Ok((jpeg, "image/jpeg"))
    } else {
        Err("image is too large to send".into())
    }
}

/// JPEG has no alpha channel: composite onto white first.
fn flatten(pixbuf: &Pixbuf) -> Pixbuf {
    let (w, h) = (pixbuf.width(), pixbuf.height());
    pixbuf.composite_color_simple(w, h, InterpType::Nearest, 255, 32, 0xffffff, 0xffffff).unwrap_or_else(|| pixbuf.clone())
}

/// Captures a rectangle of the screen (layout coordinates) with `grim`.
pub fn capture_rect((x, y, w, h): (i64, i64, i64, i64)) -> Result<Option<Vec<u8>>, String> {
    let shot = Command::new("grim")
        .args(["-g", &format!("{x},{y} {w}x{h}"), "-t", "png", "-"])
        .output()
        .map_err(|e| format!("couldn't run grim ({e}); is it installed?"))?;
    if !shot.status.success() {
        return Err(format!("grim failed: {}", String::from_utf8_lossy(&shot.stderr).trim()));
    }
    Ok(Some(shot.stdout))
}

/// The text currently selected anywhere (the primary selection), via
/// wl-paste: it works without keyboard focus, which Clawd never has when a
/// hotkey fires. Blocks briefly; call off the main thread.
pub fn selected_text() -> Option<String> {
    let out = Command::new("wl-paste").args(["--primary", "--no-newline", "--type", "text"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    (out.status.success() && !text.trim().is_empty()).then_some(text)
}

/// Lets the user drag out a region with `slurp` and captures it with `grim`.
/// Returns `Ok(None)` if the selection was cancelled (Esc). Blocks, so call
/// it off the main thread.
pub fn capture_region() -> Result<Option<Vec<u8>>, String> {
    let selection = Command::new("slurp")
        .output()
        .map_err(|e| format!("couldn't run slurp ({e}); is it installed?"))?;
    if !selection.status.success() {
        return Ok(None);
    }
    let geometry = String::from_utf8_lossy(&selection.stdout).trim().to_string();
    let shot = Command::new("grim")
        .args(["-g", &geometry, "-t", "png", "-"])
        .output()
        .map_err(|e| format!("couldn't run grim ({e}); is it installed?"))?;
    if !shot.status.success() {
        return Err(format!("grim failed: {}", String::from_utf8_lossy(&shot.stderr).trim()));
    }
    Ok(Some(shot.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: i32, h: i32) -> Vec<u8> {
        let pixbuf = Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, false, 8, w, h).unwrap();
        pixbuf.fill(0xd9775700);
        pixbuf.save_to_bufferv("png", &[]).unwrap()
    }

    #[test]
    fn small_images_pass_through_as_png() {
        let a = prepare(&png(200, 100)).unwrap();
        assert_eq!(a.image.media_type, "image/png");
        assert_eq!(a.size, (200, 100));
    }

    #[test]
    fn large_images_are_downscaled() {
        let a = prepare(&png(3000, 1500)).unwrap();
        assert_eq!(a.size, (MAX_EDGE, MAX_EDGE / 2));
    }

    #[test]
    fn thumbnails_have_display_size() {
        let a = prepare(&png(640, 400)).unwrap();
        assert_eq!((a.thumb.width(), a.thumb.height()), THUMB);
        assert_eq!((a.preview.width(), a.preview.height()), (200, 125));
        // Tall images fit by height.
        let a = prepare(&png(300, 900)).unwrap();
        assert_eq!((a.preview.width(), a.preview.height()), (50, 150));
    }

    #[test]
    fn rejects_non_images() {
        assert!(prepare(b"definitely not a png").is_err());
    }
}
