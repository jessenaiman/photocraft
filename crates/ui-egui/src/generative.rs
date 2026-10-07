//! Generative Fill through the user's fal.ai key.
//!
//! The selection and a crop of the composite are sent to FLUX.1 Fill. Two results come back.
//! The bar's first choice is the original selection; the next two are those results, on one
//! layer that is hidden while the original is showing. Remove does not use this path.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Instant;

use base64::Engine as _;
use photocraft_color::PixelFormat;
use photocraft_doc::{Document, Layer};
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use serde_json::{Value, json};

use crate::PhotocraftApp;

#[cfg(all(not(test), not(target_arch = "wasm32")))]
const ENDPOINT: &str = "https://fal.run/fal-ai/flux-pro/v1/fill";
const PAD: i32 = 48;
const MAX_SIDE: u32 = 1536;
const MAX_PIXELS: u64 = 8_000_000;
const MAX_PROMPT: usize = 2_000;
const MAX_BODY: usize = 18_000_000;

/// A request that has been sent and is waiting for the images.
pub(crate) struct Pending {
    rx: Receiver<Result<Vec<Vec<u8>>, String>>,
    doc: photocraft_doc::DocId,
    crop: Rect,
    selection: Surface,
    started: Instant,
}

/// The original selection plus the generated images the bar can step through.
pub(crate) struct Variations {
    doc: photocraft_doc::DocId,
    bounds: Rect,
    crop: Rect,
    selection: Surface,
    layer: Option<photocraft_doc::LayerId>,
    images: Vec<Vec<u8>>,
    index: usize,
}

struct Prepared {
    body: Value,
    doc: photocraft_doc::DocId,
    crop: Rect,
    selection: Surface,
}

/// Build the fal.ai body from the open document. The API key is not part of it.
fn prepare(doc: &Document, selection: &Surface, prompt: &str) -> Result<Prepared, String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("Describe what Generative Fill should create.".into());
    }
    if prompt.chars().count() > MAX_PROMPT {
        return Err("That prompt is too long.".into());
    }
    let bounds = photocraft_compose::bounds::content_bounds(selection);
    if bounds.is_empty() {
        return Err("the selection is empty".into());
    }
    let canvas = doc.bounds();
    let crop =
        Rect::new((bounds.x0 - PAD).max(canvas.x0), (bounds.y0 - PAD).max(canvas.y0), (bounds.x1 + PAD).min(canvas.x1), (bounds.y1 + PAD).min(canvas.y1));
    if crop.is_empty() {
        return Err("the selection is outside the image".into());
    }
    let cw = crop.width();
    let ch = crop.height();
    let pixels = u64::from(cw).saturating_mul(u64::from(ch));
    if pixels > MAX_PIXELS {
        return Err("The selection is too large to generate. Use a smaller selection.".into());
    }
    let longest = cw.max(ch).max(1);
    let scale = (MAX_SIDE as f32 / longest as f32).min(1.0);
    let sent_w = ((cw as f32 * scale).round() as u32).max(1);
    let sent_h = ((ch as f32 * scale).round() as u32).max(1);
    let image = downsample(doc, crop, cw, ch, sent_w, sent_h)?;
    let mask = mask_png_pixels(selection, crop, cw, ch, sent_w, sent_h)?;
    let image_uri = data_uri(&png(sent_w, sent_h, image)?);
    let mask_uri = data_uri(&png(sent_w, sent_h, mask)?);
    if image_uri.len().saturating_add(mask_uri.len()) > MAX_BODY {
        return Err("The selection is too large to send.".into());
    }
    Ok(Prepared {
        body: json!({
            "prompt": prompt,
            "image_url": image_uri,
            "mask_url": mask_uri,
            "sync_mode": true,
            "output_format": "png",
            "num_images": 1,
            "safety_tolerance": "5"
        }),
        doc: doc.id,
        crop,
        selection: selection.clone(),
    })
}

/// Start a fill. In tests the request is recorded and nothing is sent.
pub(crate) fn start(app: &mut PhotocraftApp, prompt: &str) -> Result<(), String> {
    if app.generative.is_some() {
        return Err("Generative Fill is still running.".into());
    }
    let prepared = {
        let st = app.session.active().ok_or_else(|| "no document".to_string())?;
        let selection = st.doc.selection.clone().ok_or_else(|| "no selection".to_string())?;
        prepare(&st.doc, &selection, prompt)?
    };
    launch(app, prepared)
}

/// Apply a finished request, if one has arrived.
pub(crate) fn poll(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(pending) = app.generative.take() else { return };
    ctx.request_repaint_after(std::time::Duration::from_millis(50));
    match pending.rx.try_recv() {
        Err(TryRecvError::Empty) => app.generative = Some(pending),
        Err(TryRecvError::Disconnected) => fail(app, "Generative Fill stopped before it returned an image."),
        Ok(Err(e)) => fail(app, &e),
        Ok(Ok(pngs)) => match accept(app, pngs, pending.doc, pending.crop, pending.selection) {
            Ok(()) => {
                app.ui.status = "Use the arrows to compare the original with the generated images.".into();
                app.ui.status_error = false;
                app.sync_views();
            }
            Err(e) => fail(app, &e),
        },
    }
}

/// `(index, total)` while the selection still has results. Index 0 is the original.
pub(crate) fn variation_state(app: &PhotocraftApp) -> Option<(usize, usize)> {
    app.variations.as_ref().map(|v| (v.index, v.images.len().saturating_add(1)))
}

/// Show the original (`0`) or a generated image (`1..`).
pub(crate) fn show_variation(app: &mut PhotocraftApp, index: usize) -> Result<(), String> {
    let Some(v) = app.variations.as_ref() else { return Ok(()) };
    let index = index.min(v.images.len());
    if index == v.index && (index == 0 || v.layer.is_some()) {
        return Ok(());
    }
    let doc_id = v.doc;
    let crop = v.crop;
    let selection = v.selection.clone();
    let existing = v.layer;
    let png = (index > 0).then(|| v.images[index - 1].clone());
    let layer_id = if let Some(png) = png {
        let surface = masked_surface(app, &png, doc_id, crop, &selection)?;
        app.session
            .edit("Generative Fill", move |doc, active| {
                if doc.id != doc_id {
                    return Err(photocraft_engine::EngineError::Other("the document changed".into()));
                }
                if let Some(id) = existing
                    && let Some(layer) = doc.layer_mut(id)
                {
                    layer.visible = true;
                    if let Some(slot) = layer.surface_mut() {
                        *slot = surface;
                        *active = Some(id);
                        return Ok(id);
                    }
                }
                let mut layer = Layer::raster(doc.next_layer_name("Generative Fill"), surface.format());
                let Some(slot) = layer.surface_mut() else {
                    return Err(photocraft_engine::EngineError::Other("could not create the layer".into()));
                };
                *slot = surface;
                let id = doc.insert_above(*active, layer);
                *active = Some(id);
                Ok(id)
            })
            .map_err(|e| e.to_string())?
    } else if let Some(id) = existing {
        app.session
            .edit("Generative Fill", |doc, _| {
                if let Some(layer) = doc.layer_mut(id) {
                    layer.visible = false;
                }
                Ok(id)
            })
            .map_err(|e| e.to_string())?
    } else {
        if let Some(v) = app.variations.as_mut() {
            v.index = 0;
        }
        return Ok(());
    };
    if let Some(v) = app.variations.as_mut() {
        v.layer = Some(layer_id);
        v.index = index;
    }
    app.sync_views();
    Ok(())
}

/// Drop the pager when the selection is gone, and remove a hidden result layer.
pub(crate) fn sync_variations(app: &mut PhotocraftApp) {
    let stale = app.variations.as_ref().is_some_and(|v| !matches_selection(app, v));
    if !stale {
        return;
    }
    let Some(v) = app.variations.take() else { return };
    if v.index == 0
        && let Some(id) = v.layer
    {
        let _ = app.session.execute("layer.delete", json!({ "layer": id.0 }));
    }
}

/// Progress window while fal.ai is working. Cancel discards the result.
pub(crate) fn loading(app: &mut PhotocraftApp, ctx: &egui::Context) {
    let Some(started) = app.generative.as_ref().map(|p| p.started) else { return };
    ctx.request_repaint_after(std::time::Duration::from_millis(50));
    let t = crate::theme::Tokens::get(ctx);
    let mut cancel_it = ctx.input(|i| i.key_pressed(egui::Key::Escape));
    let tip = if (started.elapsed().as_secs() / 6).is_multiple_of(2) {
        tl!("The first image is your original. The next two are new versions of the selection.")
    } else {
        tl!("The arrows on the bar move between the original and each generated image.")
    };
    let modal = egui::Modal::new(egui::Id::new("generative-fill-progress")).backdrop_color(egui::Color32::from_black_alpha(48)).show(ctx, |ui| {
        ui.set_width(420.0);
        ui.label(egui::RichText::new(tl!("Generating with Generative Fill")).font(crate::theme::semibold(16.0)).color(t.text));
        ui.add_space(14.0);
        let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
        progress_bar(ui, bar, ease(started.elapsed().as_secs_f32()), &t);
        ui.add_space(14.0);
        ui.vertical_centered(|ui| {
            ui.set_max_width(400.0);
            ui.label(egui::RichText::new(tip).italics().size(13.5).color(t.text_dim));
        });
        ui.add_space(16.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if crate::widgets::secondary_button(ui, tl!("Cancel"), 96.0).clicked() {
                cancel_it = true;
            }
        });
    });
    let _ = modal;
    if cancel_it {
        cancel(app);
    }
}

fn ease(seconds: f32) -> f32 {
    (1.0 - (-seconds.max(0.0) / 8.0).exp()) * 0.9
}

fn progress_bar(ui: &egui::Ui, rect: egui::Rect, frac: f32, t: &crate::theme::Tokens) {
    let radius = rect.height() / 2.0;
    ui.painter().rect_filled(rect, radius, t.field);
    let w = (rect.width() * frac.clamp(0.04, 0.92)).max(8.0);
    ui.painter().rect_filled(egui::Rect::from_min_size(rect.min, egui::vec2(w.min(rect.width()), rect.height())), radius, t.accent);
}

fn cancel(app: &mut PhotocraftApp) {
    app.generative = None;
    app.ui.status = "Generative Fill was cancelled.".into();
    app.ui.status_error = false;
}

pub(crate) fn accept(app: &mut PhotocraftApp, pngs: Vec<Vec<u8>>, doc: photocraft_doc::DocId, crop: Rect, selection: Surface) -> Result<(), String> {
    if pngs.is_empty() {
        return Err("fal.ai did not return an image".into());
    }
    let bounds = photocraft_compose::bounds::content_bounds(&selection);
    app.variations = Some(Variations { doc, bounds, crop, selection, layer: None, images: pngs, index: 0 });
    if let Err(e) = show_variation(app, 1) {
        app.variations = None;
        return Err(e);
    }
    Ok(())
}

fn matches_selection(app: &PhotocraftApp, v: &Variations) -> bool {
    let Some(st) = app.session.active() else { return false };
    if st.doc.id != v.doc {
        return false;
    }
    let Some(sel) = st.doc.selection.as_ref() else { return false };
    photocraft_compose::bounds::content_bounds(sel) == v.bounds
}

fn fail(app: &mut PhotocraftApp, message: &str) {
    app.ui.status = message.to_string();
    app.ui.status_error = true;
}

fn launch(app: &mut PhotocraftApp, prepared: Prepared) -> Result<(), String> {
    #[cfg(test)]
    {
        LAST_BODY.with(|slot| *slot.borrow_mut() = Some(prepared.body));
        let (tx, rx) = std::sync::mpsc::channel();
        app.generative = Some(Pending { rx, doc: prepared.doc, crop: prepared.crop, selection: prepared.selection, started: Instant::now() });
        std::mem::forget(tx);
        Ok(())
    }
    #[cfg(all(not(test), target_arch = "wasm32"))]
    {
        let _ = (app, prepared);
        Err("Generative Fill needs the desktop app.".into())
    }
    #[cfg(all(not(test), not(target_arch = "wasm32")))]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let key = crate::contextual_bar::api_key(app);
        let body = prepared.body;
        std::thread::Builder::new()
            .name("generative-fill".into())
            .spawn(move || {
                let _ = tx.send(post(&key, &body).map_err(|e| hide_key(e, &key)));
            })
            .map_err(|_| "could not start Generative Fill".to_string())?;
        app.generative = Some(Pending { rx, doc: prepared.doc, crop: prepared.crop, selection: prepared.selection, started: Instant::now() });
        Ok(())
    }
}

fn downsample(doc: &Document, crop: Rect, cw: u32, ch: u32, sent_w: u32, sent_h: u32) -> Result<Vec<u8>, String> {
    let mut out = vec![255u8; sent_w as usize * sent_h as usize * 4];
    photocraft_compose::render_bands(doc, crop, 0, |band| -> Result<(), String> {
        let bw = band.rect.width();
        if bw == 0 {
            return Ok(());
        }
        let bw = bw as usize;
        for (i, p) in band.px.iter().enumerate() {
            let x = band.rect.x0.saturating_add((i % bw) as i32);
            let y = band.rect.y0.saturating_add((i / bw) as i32);
            let ox = u32::try_from((x - crop.x0).max(0)).unwrap_or(0).saturating_mul(sent_w) / cw;
            let oy = u32::try_from((y - crop.y0).max(0)).unwrap_or(0).saturating_mul(sent_h) / ch;
            if ox >= sent_w || oy >= sent_h {
                continue;
            }
            let o = (oy as usize * sent_w as usize + ox as usize) * 4;
            let a = p[3].clamp(0.0, 1.0);
            for c in 0..3 {
                let v = p[c].clamp(0.0, 1.0) * a + (1.0 - a);
                out[o + c] = (v * 255.0 + 0.5) as u8;
            }
            out[o + 3] = 255;
        }
        Ok(())
    })?;
    Ok(out)
}

fn mask_png_pixels(selection: &Surface, crop: Rect, cw: u32, ch: u32, sent_w: u32, sent_h: u32) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; sent_w as usize * sent_h as usize * 4];
    let mut any = false;
    for oy in 0..sent_h {
        for ox in 0..sent_w {
            let dx = crop.x0 + (ox as i32 * cw as i32) / sent_w as i32;
            let dy = crop.y0 + (oy as i32 * ch as i32) / sent_h as i32;
            let on = selection.sample_channel(dx, dy, 0) > 0.5;
            any |= on;
            let v = if on { 255 } else { 0 };
            let o = (oy as usize * sent_w as usize + ox as usize) * 4;
            out[o] = v;
            out[o + 1] = v;
            out[o + 2] = v;
            out[o + 3] = 255;
        }
    }
    if !any {
        return Err("the selection is empty".into());
    }
    Ok(out)
}

fn png(w: u32, h: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let image = photocraft_codecs::Image::from_u8(w, h, photocraft_codecs::ChannelLayout::Rgba, rgba).map_err(|e| e.to_string())?;
    photocraft_codecs::encode(&image, photocraft_codecs::Format::Png, &Default::default()).map_err(|e| e.to_string())
}

fn data_uri(png_bytes: &[u8]) -> String {
    let mut s = String::from("data:image/png;base64,");
    base64::engine::general_purpose::STANDARD.encode_string(png_bytes, &mut s);
    s
}

fn masked_surface(app: &PhotocraftApp, png_bytes: &[u8], doc_id: photocraft_doc::DocId, crop: Rect, selection: &Surface) -> Result<Surface, String> {
    let rgba = decode_rgba(png_bytes)?;
    let cw = crop.width();
    let ch = crop.height();
    if cw == 0 || ch == 0 {
        return Err("the selection is empty".into());
    }
    let fitted = fit_rgba(rgba.0, rgba.1, &rgba.2, cw, ch);
    let mut masked = fitted;
    let mut any = false;
    for y in 0..ch {
        for x in 0..cw {
            let o = (y as usize * cw as usize + x as usize) * 4;
            if selection.sample_channel(crop.x0 + x as i32, crop.y0 + y as i32, 0) > 0.5 {
                masked[o + 3] = 255;
                any = true;
            } else {
                masked[o + 3] = 0;
            }
        }
    }
    if !any {
        return Err("the selection is empty".into());
    }
    let need = (cw as usize).checked_mul(ch as usize).and_then(|n| n.checked_mul(4)).ok_or_else(|| "the selection is too large".to_string())?;
    if masked.len() != need {
        return Err("the generated image has the wrong size".into());
    }
    let mut surface = Surface::from_interleaved(PixelFormat::RGBA8, crop, &masked);
    surface.prune();
    let st = app.session.active().ok_or_else(|| "no document".to_string())?;
    if st.doc.id != doc_id {
        return Err("the document changed".into());
    }
    let f = st.doc.pixel_format();
    let target = PixelFormat::new(f.mode, f.sample, true);
    if surface.format() != target {
        surface = surface.convert(target);
    }
    Ok(surface)
}

fn decode_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut opts = photocraft_codecs::DecodeOptions::default();
    opts.limits.max_width = 4096;
    opts.limits.max_height = 4096;
    opts.limits.max_pixels = 16_000_000;
    let image = photocraft_codecs::decode_with(bytes, &opts).map_err(|e| e.to_string())?;
    let w = image.width();
    let h = image.height();
    if w == 0 || h == 0 {
        return Err("fal.ai returned an empty image".into());
    }
    let rgba = image.to_rgba8();
    let need = (w as usize).checked_mul(h as usize).and_then(|n| n.checked_mul(4)).ok_or_else(|| "fal.ai returned an image that is too large".to_string())?;
    if rgba.len() != need {
        return Err("fal.ai returned an image that could not be read".into());
    }
    Ok((w, h, rgba))
}

fn fit_rgba(src_w: u32, src_h: u32, src: &[u8], dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h && src.len() == dst_w as usize * dst_h as usize * 4 {
        return src.to_vec();
    }
    let mut dst = vec![0u8; dst_w as usize * dst_h as usize * 4];
    if src_w == 0 || src_h == 0 || src.len() < 4 {
        return dst;
    }
    for y in 0..dst_h {
        let sy = (u64::from(y) * u64::from(src_h) / u64::from(dst_h.max(1))).min(u64::from(src_h - 1)) as u32;
        for x in 0..dst_w {
            let sx = (u64::from(x) * u64::from(src_w) / u64::from(dst_w.max(1))).min(u64::from(src_w - 1)) as u32;
            let s = (sy as usize * src_w as usize + sx as usize) * 4;
            let d = (y as usize * dst_w as usize + x as usize) * 4;
            if s + 4 <= src.len() && d + 4 <= dst.len() {
                dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
            }
        }
    }
    dst
}

/// Pull up to two images out of a fal.ai JSON body. `https` results are downloaded by [`post`].
fn images_from_body(v: &Value) -> Result<Vec<ImageRef<'_>>, String> {
    let Some(items) = v.get("images").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
        return Err(explain_body(v));
    };
    let mut out = Vec::new();
    for item in items.iter().take(2) {
        let url = item.get("url").and_then(Value::as_str).ok_or_else(|| "fal.ai returned an image that could not be read".to_string())?;
        out.push(image_ref(url)?);
    }
    Ok(out)
}

fn image_ref(url: &str) -> Result<ImageRef<'_>, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        let b64 = rest.split_once(',').map(|(_, data)| data).filter(|s| !s.is_empty()).ok_or_else(|| "fal.ai returned an empty image".to_string())?;
        return Ok(ImageRef::Bytes(decode_b64(b64)?));
    }
    if trusted_https(url) {
        return Ok(ImageRef::Url(url));
    }
    Err("fal.ai returned an image address this app will not download.".into())
}

#[derive(Debug)]
enum ImageRef<'a> {
    Bytes(Vec<u8>),
    Url(&'a str),
}

fn explain_body(v: &Value) -> String {
    let detail = v.get("detail").or_else(|| v.get("error")).or_else(|| v.get("message"));
    let msg = match detail {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if msg.is_empty() { "fal.ai did not return an image".into() } else { format!("fal.ai: {}", clip(&msg)) }
}

fn clip(s: &str) -> String {
    s.chars().take(180).collect()
}

fn decode_b64(data: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data.trim()))
        .map_err(|_| "fal.ai returned an image that could not be read".into())
}

fn trusted_https(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    host == "fal.media"
        || host.ends_with(".fal.media")
        || host == "fal.ai"
        || host.ends_with(".fal.ai")
        || host == "fal.run"
        || host.ends_with(".fal.run")
        || host == "storage.googleapis.com"
}

/// Two one-image requests. Different seeds so the results are not the same picture.
fn request_pair(body: &Value, seed: u64) -> [Value; 2] {
    let mut first = body.clone();
    let mut second = body.clone();
    first["num_images"] = json!(1);
    second["num_images"] = json!(1);
    first["seed"] = json!(seed);
    second["seed"] = json!(seed.wrapping_add(1));
    [first, second]
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn fresh_seed() -> u64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
    u64::try_from(nanos % 0x8000_0000).unwrap_or(1).max(1)
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn hide_key(message: String, key: &str) -> String {
    if key.is_empty() { message } else { message.replace(key, "…") }
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn post(key: &str, body: &Value) -> Result<Vec<Vec<u8>>, String> {
    // One call returns one image. Two calls with different seeds give the two variations.
    let [first, second] = request_pair(body, fresh_seed());
    let key_a = key.to_string();
    let key_b = key.to_string();
    let left = std::thread::Builder::new()
        .name("generative-fill-a".into())
        .spawn(move || post_one(&key_a, &first))
        .map_err(|_| "could not start Generative Fill".to_string())?;
    let right = std::thread::Builder::new()
        .name("generative-fill-b".into())
        .spawn(move || post_one(&key_b, &second))
        .map_err(|_| "could not start Generative Fill".to_string())?;
    let a = join_image(left)?;
    let b = join_image(right)?;
    Ok(vec![a, b])
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn join_image(handle: std::thread::JoinHandle<Result<Vec<u8>, String>>) -> Result<Vec<u8>, String> {
    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("Generative Fill stopped before it returned an image.".into()),
    }
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn post_one(key: &str, body: &Value) -> Result<Vec<u8>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(180))).build().into();
    let mut response = agent.post(ENDPOINT).header("Authorization", &format!("Key {key}")).send_json(body).map_err(|e| hide_key(e.to_string(), key))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string().map_err(|e| hide_key(e.to_string(), key))?;
    if text.len() > 32 * 1024 * 1024 {
        return Err("fal.ai returned a response that is too large".into());
    }
    if !(200..300).contains(&status) {
        return Err(hide_key(http_error(status, &text), key));
    }
    let value: Value = serde_json::from_str(&text).map_err(|_| "fal.ai returned something that is not JSON".to_string())?;
    let image = images_from_body(&value)?.into_iter().next().ok_or_else(|| "fal.ai did not return an image".to_string())?;
    match image {
        ImageRef::Bytes(bytes) => Ok(bytes),
        ImageRef::Url(url) => download(&agent, url),
    }
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn download(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let mut response = agent.get(url).call().map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("could not download the generated image ({status})"));
    }
    let bytes = response.body_mut().read_to_vec().map_err(|e| e.to_string())?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("the generated image is too large".into());
    }
    Ok(bytes)
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn http_error(status: u16, text: &str) -> String {
    if status == 401 || status == 403 {
        return "fal.ai rejected the API key.".into();
    }
    let value: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let detail = explain_body(&value);
    if detail == "fal.ai did not return an image" { format!("fal.ai returned status {status}") } else { detail }
}

#[cfg(test)]
std::thread_local! {
    static LAST_BODY: std::cell::RefCell<Option<Value>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn take_test_body() -> Option<Value> {
    LAST_BODY.with(|slot| slot.borrow_mut().take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc_with_selection() -> (Document, Surface) {
        let mut session = photocraft_engine::Session::new();
        session.execute("file.new", json!({ "width": 80, "height": 60 })).unwrap();
        session.execute("select.rect", json!({ "x": 20, "y": 16, "width": 10, "height": 8 })).unwrap();
        let doc = session.active().unwrap().doc.clone();
        let selection = doc.selection.clone().unwrap();
        (doc.as_ref().clone(), selection)
    }

    #[test]
    fn the_request_masks_the_selection_and_leaves_out_the_key() {
        let (doc, selection) = doc_with_selection();
        let prepared = prepare(&doc, &selection, "a red balloon").unwrap();
        assert_eq!(prepared.body["prompt"], "a red balloon");
        assert_eq!(prepared.body["sync_mode"], true);
        assert_eq!(prepared.body["num_images"], 1);
        let [first, second] = request_pair(&prepared.body, 40);
        assert_eq!(first["seed"], 40);
        assert_eq!(second["seed"], 41);
        assert_eq!(first["prompt"], second["prompt"]);
        assert_eq!(first["num_images"], 1);
        assert!(!prepared.body.to_string().contains("secret-key"));
        let mask = png_from_uri(prepared.body["mask_url"].as_str().unwrap());
        let (w, h, px) = decode_rgba(&mask).unwrap();
        let at = |x: u32, y: u32| px[((y * w + x) * 4) as usize];
        assert!(at(20, 16) == 255, "inside the selection is white");
        assert_eq!(at(2, 2), 0, "outside the selection is black");
        assert!(w >= 10 && h >= 8);
    }

    #[test]
    fn a_data_uri_response_is_the_image_and_a_foreign_url_is_refused() {
        let red = png(1, 1, vec![255, 0, 0, 255]).unwrap();
        let uri = data_uri(&red);
        let blue = png(1, 1, vec![0, 0, 255, 255]).unwrap();
        let body = json!({ "images": [{ "url": uri }, { "url": data_uri(&blue) }, { "url": uri }] });
        let got = images_from_body(&body).unwrap();
        assert_eq!(got.len(), 2, "only the first two images are kept");
        match &got[0] {
            ImageRef::Bytes(bytes) => assert_eq!(bytes, &red),
            ImageRef::Url(_) => panic!("data uri"),
        }
        assert!(images_from_body(&json!({ "images": [{ "url": "https://evil.example/a.png" }] })).is_err());
        match images_from_body(&json!({ "images": [{ "url": "https://v3.fal.media/files/a.png" }] })).unwrap().pop().unwrap() {
            ImageRef::Url(url) => assert_eq!(url, "https://v3.fal.media/files/a.png"),
            ImageRef::Bytes(_) => panic!("expected a download address"),
        }
        assert!(images_from_body(&json!({ "detail": "nope" })).unwrap_err().contains("nope"));
        assert!(images_from_body(&json!({})).is_err());
    }

    #[test]
    fn the_original_is_first_and_the_two_results_replace_only_the_selection() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({ "width": 16, "height": 16 })).unwrap();
        app.run("select.rect", json!({ "x": 4, "y": 4, "width": 4, "height": 4 })).unwrap();
        let doc_id = app.session.active().unwrap().doc.id;
        let selection = app.session.active().unwrap().doc.selection.clone().unwrap();
        let red = png(4, 4, [255, 0, 0, 255].repeat(16)).unwrap();
        let blue = png(4, 4, [0, 0, 255, 255].repeat(16)).unwrap();
        accept(&mut app, vec![red, blue], doc_id, Rect::new(4, 4, 8, 8), selection).unwrap();
        assert_eq!(variation_state(&app), Some((1, 3)));
        let inside = photocraft_compose::render(&app.session.active().unwrap().doc, Rect::new(5, 5, 6, 6));
        assert!(inside.px[0][0] > 0.9, "{:?}", inside.px[0]);
        show_variation(&mut app, 2).unwrap();
        assert_eq!(variation_state(&app), Some((2, 3)));
        let inside = photocraft_compose::render(&app.session.active().unwrap().doc, Rect::new(5, 5, 6, 6));
        assert!(inside.px[0][2] > 0.9 && inside.px[0][0] < 0.1, "{:?}", inside.px[0]);
        show_variation(&mut app, 0).unwrap();
        assert_eq!(variation_state(&app), Some((0, 3)));
        let inside = photocraft_compose::render(&app.session.active().unwrap().doc, Rect::new(5, 5, 6, 6));
        assert!(inside.px[0][0] > 0.9 && inside.px[0][1] > 0.9, "original shows through: {:?}", inside.px[0]);
        let outside = photocraft_compose::render(&app.session.active().unwrap().doc, Rect::new(0, 0, 1, 1));
        assert!(outside.px[0][0] > 0.9 && outside.px[0][1] > 0.9, "{:?}", outside.px[0]);
        assert!((0.0..0.9).contains(&ease(0.0)));
        assert!((0.8..0.91).contains(&ease(80.0)));
    }

    fn png_from_uri(uri: &str) -> Vec<u8> {
        let b64 = uri.split_once(',').unwrap().1;
        decode_b64(b64).unwrap()
    }
}
