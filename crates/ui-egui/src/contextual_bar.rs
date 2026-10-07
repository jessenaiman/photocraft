//! The selection contextual task bar: the floating row under a marching-ants selection.
//!
//! Generative Fill keeps a gear beside it because the model is the user's. fal.ai is the key
//! this bar asks for: one key runs SAM 3 (`https://fal.run/fal-ai/sam-3/image`), which segments
//! the object inside the selection before Remove fills it, and the same key is what Generative
//! Fill will send a prompt with. Remove still works with no key, through content-aware fill.

use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, pos2, vec2};
use photocraft_geom::Rect as DocRect;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;

const PREF: &str = "contextual.generative";
const FAL_KEYS: &str = "https://fal.ai/dashboard/keys";
const SAM3_ENDPOINT: &str = "https://fal.run/fal-ai/sam-3/image";
const GAP: f32 = 8.0;

/// Where the bar sits, and which popover is open. The API key is not here; it lives in preferences.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BarState {
    pub pinned: bool,
    pub offset: [f32; 2],
    pub pinned_pos: [f32; 2],
    #[serde(skip)]
    pop: Popover,
    #[serde(skip)]
    pub prompt: String,
    #[serde(skip)]
    pub last_size: [f32; 2],
}

impl Default for BarState {
    fn default() -> Self {
        Self { pinned: false, offset: [0.0, 0.0], pinned_pos: [0.0, 0.0], pop: Popover::None, prompt: String::new(), last_size: [0.0, 0.0] }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Popover {
    #[default]
    None,
    Fill,
    Gear,
    Modify,
    Adjust,
    More,
}

/// A SAM 3 image request. `body` is the JSON fal.ai expects; the API key is a header, never a field.
struct Sam3Call {
    endpoint: &'static str,
    body: Value,
}

/// Box prompt for SAM 3, in document pixels, clamped to the image. `image_url` is filled in when
/// the pixels are uploaded; this only checks that the selection is a real box.
fn sam3_request(width: u32, height: u32, bounds: DocRect, image_url: &str) -> Result<Sam3Call, String> {
    if width == 0 || height == 0 {
        return Err("the image has no pixels".into());
    }
    if bounds.is_empty() {
        return Err("the selection is empty".into());
    }
    if image_url.len() > 2_000_000 {
        return Err("the image address is too long".into());
    }
    let max_x = i32::try_from(width).unwrap_or(i32::MAX);
    let max_y = i32::try_from(height).unwrap_or(i32::MAX);
    let x0 = bounds.x0.clamp(0, max_x);
    let y0 = bounds.y0.clamp(0, max_y);
    let x1 = bounds.x1.clamp(0, max_x);
    let y1 = bounds.y1.clamp(0, max_y);
    if x1 <= x0 || y1 <= y0 {
        return Err("the selection is outside the image".into());
    }
    Ok(Sam3Call {
        endpoint: SAM3_ENDPOINT,
        body: json!({
            "image_url": image_url,
            "box_prompts": [{ "x_min": x0, "y_min": y0, "x_max": x1, "y_max": y1 }],
            "apply_mask": false,
            "sync_mode": true,
            "output_format": "png",
            "include_scores": true,
            "return_multiple_masks": false
        }),
    })
}

fn api_key(app: &PhotocraftApp) -> String {
    app.session
        .prefs()
        .dialogs
        .get(PREF)
        .and_then(|v| v.get("apiKey"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

fn save_api_key(app: &mut PhotocraftApp, key: &str) {
    let key = key.trim();
    app.session.prefs.edit(|p| {
        if key.is_empty() {
            p.dialogs.remove(PREF);
        } else {
            p.dialogs.insert(PREF.into(), json!({ "apiKey": key }));
        }
    });
}

/// `Some` when a key is saved and the selection is a box SAM 3 can take. `None` when there is no key.
fn sam_plan(app: &PhotocraftApp) -> Result<Option<Sam3Call>, String> {
    if api_key(app).is_empty() {
        return Ok(None);
    }
    let st = app.session.active().ok_or_else(|| "no document".to_string())?;
    let sel = st.doc.selection.as_ref().ok_or_else(|| "no selection".to_string())?;
    let bounds = photocraft_compose::bounds::content_bounds(sel);
    sam3_request(st.doc.size.width, st.doc.size.height, bounds, "").map(Some)
}

/// Top-left of the bar. Unpinned, it sits under the selection; pinned, it stays where it was put.
pub fn bar_origin(selection: Rect, canvas: Rect, bar: Vec2, offset: [f32; 2], pinned: bool, pinned_pos: [f32; 2]) -> Pos2 {
    let raw =
        if pinned { pos2(pinned_pos[0], pinned_pos[1]) } else { pos2(selection.center().x - bar.x * 0.5 + offset[0], selection.bottom() + GAP + offset[1]) };
    let max_x = (canvas.right() - bar.x - 4.0).max(canvas.left() + 4.0);
    let max_y = (canvas.bottom() - bar.y - 4.0).max(canvas.top() + 4.0);
    pos2(raw.x.clamp(canvas.left() + 4.0, max_x), raw.y.clamp(canvas.top() + 4.0, max_y))
}

fn selection_screen(app: &PhotocraftApp) -> Option<Rect> {
    let idx = app.session.active_index()?;
    let st = app.session.active()?;
    let sel = st.doc.selection.as_ref()?;
    let b = photocraft_compose::bounds::content_bounds(sel);
    if b.is_empty() {
        return None;
    }
    let view = app.ui.views.get(idx)?;
    let canvas = crate::rulers::content_rect(app, app.last_canvas_rect);
    if canvas.width() < 2.0 || canvas.height() < 2.0 {
        return None;
    }
    let xf = crate::canvas::ViewXform { rect: canvas, zoom: view.zoom, center: view.center, flip: app.ui.view.flip_horizontal };
    Some(xf.doc_rect(b))
}

fn showing(app: &PhotocraftApp) -> bool {
    app.ui.panels.contextual_bar && app.ui.dialogs.is_empty() && !app.ui.view.hides_chrome() && app.ui.transform.is_none()
}

/// Draw the bar when a selection is up. Clicks stay on the bar; the canvas is underneath.
pub fn show(app: &mut PhotocraftApp, ctx: &egui::Context) {
    if !showing(app) {
        return;
    }
    let Some(selection) = selection_screen(app) else { return };
    let canvas = crate::rulers::content_rect(app, app.last_canvas_rect);
    let offset = app.ui.contextual.offset;
    let pinned = app.ui.contextual.pinned;
    let pinned_pos = app.ui.contextual.pinned_pos;
    let last = app.ui.contextual.last_size;
    let known = last[0] > 1.0 && last[1] > 1.0;
    let size = vec2(if known { last[0] } else { 640.0 }, if known { last[1] } else { 36.0 });
    let origin = bar_origin(selection, canvas, size, offset, pinned, pinned_pos);
    let shown = egui::Area::new(egui::Id::new("contextual-task-bar")).order(egui::Order::Foreground).fixed_pos(origin).show(ctx, |ui| {
        ui.spacing_mut().item_spacing = vec2(2.0, 4.0);
        let t = Tokens::get(ui.ctx());
        let frame = egui::Frame::NONE
            .fill(t.dock)
            .stroke(Stroke::new(1.0, t.card_border))
            .corner_radius(t.radius_lg)
            .inner_margin(egui::Margin { left: 6, right: 6, top: 4, bottom: 4 })
            .shadow(egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: t.shadow });
        let bar = frame
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    row(app, ui);
                });
            })
            .response
            .rect;
        app.ui.contextual.last_size = [bar.width(), bar.height()];
        match app.ui.contextual.pop {
            Popover::None => {}
            Popover::Fill => fill_pop(app, ui),
            Popover::Gear => gear_pop(app, ui),
            Popover::Modify => modify_pop(app, ui),
            Popover::Adjust => adjust_pop(app, ui),
            Popover::More => more_pop(app, ui, origin),
        }
    });
    if shown.response.clicked_elsewhere() {
        app.ui.contextual.pop = Popover::None;
    }
}

fn row(app: &mut PhotocraftApp, ui: &mut Ui) {
    let drag = grip(ui);
    if drag.dragged() {
        let d = drag.drag_delta();
        let bar = &mut app.ui.contextual;
        if bar.pinned {
            bar.pinned_pos[0] += d.x;
            bar.pinned_pos[1] += d.y;
        } else {
            bar.offset[0] += d.x;
            bar.offset[1] += d.y;
        }
    }
    let fill = tl!("Generative Fill");
    if label_btn(ui, "sparkles", fill, tl!("Modify existing content, extend images, and generate objects, backgrounds and scenes.")).clicked() {
        toggle(app, Popover::Fill);
    }
    if icon_btn(ui, "settings", tl!("Generative fill settings")).clicked() {
        toggle(app, Popover::Gear);
    }
    ui.add_space(4.0);
    if label_btn(ui, "bandage", tl!("Remove"), tl!("Remove the selected pixels. With a fal.ai key, SAM 3 segments the object first.")).clicked() {
        app.ui.contextual.pop = Popover::None;
        remove(app);
    }
    if icon_btn(ui, "brush", tl!("Modify selection")).clicked() {
        toggle(app, Popover::Modify);
    }
    if icon_btn(ui, "arrow-left-right", tl!("Invert selection")).clicked() {
        let _ = run_cmd(app, "select.inverse", json!({}));
    }
    if icon_btn(ui, "square-dashed", tl!("Create mask from selection")).clicked() {
        let _ = run_cmd(app, "layer.layerMask.revealSelection", json!({}));
    }
    if icon_btn(ui, "paint-bucket", tl!("Fill selection")).clicked() {
        let _ = run_cmd(app, "edit.fill", json!({ "contents": "foreground" }));
    }
    if icon_btn(ui, "contrast", tl!("Create new adjustment layer")).clicked() {
        toggle(app, Popover::Adjust);
    }
    if icon_btn(ui, "ellipsis", tl!("More options")).clicked() {
        toggle(app, Popover::More);
    }
    ui.add_space(8.0);
    if deselect_btn(ui).clicked() {
        let _ = run_cmd(app, "select.deselect", json!({}));
    }
}

fn toggle(app: &mut PhotocraftApp, which: Popover) {
    app.ui.contextual.pop = if app.ui.contextual.pop == which { Popover::None } else { which };
}

fn grip(ui: &mut Ui) -> Response {
    let t = Tokens::get(ui.ctx());
    let tip = tl!("Drag to move the bar");
    let (rect, resp) = ui.allocate_exact_size(vec2(16.0, 28.0), Sense::click_and_drag());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
    let color = if resp.hovered() || resp.dragged() { t.text } else { t.text_faint };
    let c = rect.center();
    for row in -1..=1 {
        for col in [-1.0, 1.0] {
            ui.painter().circle_filled(pos2(c.x + col * 2.5, c.y + row as f32 * 3.5), 1.15, color);
        }
    }
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::Grab)
}

fn label_btn(ui: &mut Ui, icon: &str, label: &str, tip: &str) -> Response {
    let t = Tokens::get(ui.ctx());
    let galley = ui.painter().layout_no_wrap(label.to_owned(), egui::FontId::proportional(12.5), t.text);
    let w = galley.size().x + 36.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label));
    let fill = if resp.hovered() { t.hover } else { t.card };
    ui.painter().rect_filled(rect, t.radius_sm, fill);
    let icon_rect = Rect::from_center_size(pos2(rect.left() + 16.0, rect.center().y), vec2(16.0, 16.0));
    crate::icons::paint(ui, icon_rect, icon, 15.0, t.icon);
    ui.painter().galley(pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0), galley, t.text);
    resp.on_hover_text(tip)
}

fn icon_btn(ui: &mut Ui, icon: &str, tip: &str) -> Response {
    let t = Tokens::get(ui.ctx());
    let (rect, resp) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), tip));
    if resp.hovered() {
        ui.painter().rect_filled(rect, t.radius_sm, t.hover);
    }
    crate::icons::paint(ui, rect, icon, 15.0, if resp.hovered() { t.text } else { t.icon });
    resp.on_hover_text(tip)
}

fn deselect_btn(ui: &mut Ui) -> Response {
    let t = Tokens::get(ui.ctx());
    let label = tl!("Deselect");
    let galley = ui.painter().layout_no_wrap(label.to_owned(), egui::FontId::proportional(12.5), t.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 22.0, 28.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), label));
    ui.painter().rect_filled(rect, t.radius_lg, if resp.hovered() { t.hover } else { Color32::TRANSPARENT });
    ui.painter().rect_stroke(rect, t.radius_lg, Stroke::new(1.0, t.field_border), egui::StrokeKind::Inside);
    ui.painter().galley(pos2(rect.center().x - galley.size().x / 2.0, rect.center().y - galley.size().y / 2.0), galley, t.text);
    resp
}

fn pop_frame(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(4.0);
    egui::Frame::NONE
        .fill(t.card)
        .stroke(Stroke::new(1.0, t.card_border))
        .corner_radius(t.radius)
        .inner_margin(egui::Margin { left: 10, right: 10, top: 8, bottom: 8 })
        .show(ui, |ui| {
            ui.set_max_width(320.0);
            body(ui);
        });
}

fn menu_btn(ui: &mut Ui, label: &str) -> bool {
    ui.add(egui::Button::new(label).frame(false).min_size(vec2(200.0, 26.0))).clicked()
}

fn fill_pop(app: &mut PhotocraftApp, ui: &mut Ui) {
    pop_frame(ui, |ui| {
        ui.label(egui::RichText::new(tl!("Generative Fill")).color(Tokens::get(ui.ctx()).text));
        ui.add_space(4.0);
        let edit = egui::TextEdit::multiline(&mut app.ui.contextual.prompt).desired_rows(2).hint_text(tl!("Describe what to generate in the selection."));
        ui.add(edit);
        ui.add_space(4.0);
        if crate::widgets::primary_button(ui, "Generate", 96.0).clicked() {
            generate(app);
        }
    });
}

fn gear_pop(app: &mut PhotocraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    pop_frame(ui, |ui| {
        ui.label(egui::RichText::new(tl!("Generative fill settings")).color(t.text));
        ui.add_space(4.0);
        #[rustfmt::skip]
        let blurb = tl!("A fal.ai API key is used for Generative Fill, and to find the object in the selection before Remove. The key is stored only on this computer.");
        ui.label(egui::RichText::new(blurb).size(12.0).color(t.text_dim));
        ui.add_space(8.0);
        let mut key = api_key(app);
        let edit = egui::TextEdit::singleline(&mut key).password(true).hint_text(tl!("fal.ai API key")).desired_width(280.0);
        if ui.add(edit).changed() {
            save_api_key(app, &key);
        }
        if ui.add(egui::Button::new(tl!("Get a fal.ai key")).frame(false)).clicked() {
            crate::links::open(app, ui.ctx(), FAL_KEYS);
        }
    });
}

fn modify_pop(app: &mut PhotocraftApp, ui: &mut Ui) {
    pop_frame(ui, |ui| {
        for (id, label) in [
            ("select.modify.expand", tl!("Expand…")),
            ("select.modify.contract", tl!("Contract…")),
            ("select.modify.feather", tl!("Feather…")),
            ("select.modify.smooth", tl!("Smooth…")),
            ("select.modify.border", tl!("Border…")),
        ] {
            if menu_btn(ui, label) {
                run_menu(app, ui, id);
            }
        }
    });
}

fn adjust_pop(app: &mut PhotocraftApp, ui: &mut Ui) {
    pop_frame(ui, |ui| {
        for (id, label) in [
            ("layer.newAdjustmentLayer.brightnessContrast", tl!("Brightness/Contrast…")),
            ("layer.newAdjustmentLayer.levels", tl!("Levels…")),
            ("layer.newAdjustmentLayer.curves", tl!("Curves…")),
            ("layer.newAdjustmentLayer.hueSaturation", tl!("Hue/Saturation…")),
            ("layer.newAdjustmentLayer.blackWhite", tl!("Black & White…")),
            ("layer.newAdjustmentLayer.invert", tl!("Invert…")),
        ] {
            if menu_btn(ui, label) {
                run_menu(app, ui, id);
            }
        }
    });
}

fn more_pop(app: &mut PhotocraftApp, ui: &mut Ui, origin: Pos2) {
    pop_frame(ui, |ui| {
        if menu_btn(ui, tl!("Hide bar")) {
            app.ui.panels.contextual_bar = false;
            app.ui.contextual.pop = Popover::None;
        }
        if menu_btn(ui, tl!("Reset bar position")) {
            app.ui.contextual.offset = [0.0, 0.0];
            app.ui.contextual.pinned = false;
            app.ui.contextual.pop = Popover::None;
        }
        let pin = if app.ui.contextual.pinned { tl!("Unpin bar position") } else { tl!("Pin bar position") };
        if menu_btn(ui, pin) {
            if app.ui.contextual.pinned {
                app.ui.contextual.pinned = false;
            } else {
                app.ui.contextual.pinned = true;
                app.ui.contextual.pinned_pos = [origin.x, origin.y];
            }
            app.ui.contextual.pop = Popover::None;
        }
    });
}

fn generate(app: &mut PhotocraftApp) {
    if api_key(app).is_empty() {
        app.ui.contextual.pop = Popover::Gear;
        app.ui.status = "Add a fal.ai API key to use Generative Fill.".into();
        app.ui.status_error = true;
        return;
    }
    if app.ui.contextual.prompt.trim().is_empty() {
        app.ui.status = "Describe what Generative Fill should create.".into();
        app.ui.status_error = true;
        return;
    }
    app.ui.status = "The fal.ai key is saved. Generative Fill will send this prompt once the image can be uploaded.".into();
    app.ui.status_error = false;
}

fn remove(app: &mut PhotocraftApp) {
    let sam = match sam_plan(app) {
        Ok(Some(call)) => {
            debug_assert_eq!(call.endpoint, SAM3_ENDPOINT);
            debug_assert!(call.body.get("box_prompts").is_some());
            true
        }
        Ok(None) => false,
        Err(e) => {
            app.ui.status = e;
            app.ui.status_error = true;
            return;
        }
    };
    match app.run("edit.contentAwareFill", json!({})) {
        Ok(_) if sam => {
            app.ui.status = "Removed with content-aware fill. SAM 3 will segment the object once the image can be sent to fal.ai.".into();
            app.ui.status_error = false;
        }
        Ok(_) => {
            app.ui.status = "Removed with content-aware fill. Add a fal.ai key to segment the object with SAM 3 first.".into();
            app.ui.status_error = false;
        }
        Err(e) => {
            app.ui.status = e;
            app.ui.status_error = true;
        }
    }
}

fn run_cmd(app: &mut PhotocraftApp, id: &str, params: Value) -> Result<Value, String> {
    app.ui.contextual.pop = Popover::None;
    match app.run(id, params) {
        Ok(v) => Ok(v),
        Err(e) => {
            app.ui.status = e.clone();
            app.ui.status_error = true;
            Err(e)
        }
    }
}

fn run_menu(app: &mut PhotocraftApp, ui: &Ui, id: &str) {
    app.ui.contextual.pop = Popover::None;
    let ctx = ui.ctx().clone();
    if let Err(e) = crate::menus::invoke(app, &ctx, id, json!({})) {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::vec2;
    use egui_kittest::{Harness, kittest::Queryable};

    #[test]
    fn the_bar_sits_under_the_selection_and_stays_on_the_canvas() {
        let selection = Rect::from_min_max(pos2(100.0, 80.0), pos2(300.0, 160.0));
        let canvas = Rect::from_min_max(pos2(0.0, 0.0), pos2(800.0, 600.0));
        let bar = vec2(200.0, 36.0);
        let p = bar_origin(selection, canvas, bar, [0.0, 0.0], false, [0.0, 0.0]);
        assert!((p.x - 100.0).abs() < 0.1, "{p:?}");
        assert!((p.y - 168.0).abs() < 0.1, "{p:?}");
        let pinned = bar_origin(selection, canvas, bar, [40.0, 40.0], true, [12.0, 14.0]);
        assert_eq!(pinned, pos2(12.0, 14.0));
        let off = bar_origin(selection, canvas, bar, [5000.0, 5000.0], false, [0.0, 0.0]);
        assert!(off.x + bar.x <= canvas.right());
        assert!(off.y + bar.y <= canvas.bottom());
        assert!(off.x >= canvas.left());
    }

    #[test]
    fn the_sam3_request_is_a_box_and_never_the_api_key() {
        let call = sam3_request(200, 100, DocRect::new(-5, 10, 1000, 40), "https://example.invalid/a.png").unwrap();
        assert_eq!(call.endpoint, "https://fal.run/fal-ai/sam-3/image");
        assert_eq!(call.body["box_prompts"][0]["x_min"], 0);
        assert_eq!(call.body["box_prompts"][0]["y_min"], 10);
        assert_eq!(call.body["box_prompts"][0]["x_max"], 200);
        assert_eq!(call.body["box_prompts"][0]["y_max"], 40);
        assert_eq!(call.body["apply_mask"], false);
        assert!(!call.body.to_string().contains("secret-key"));
        assert!(sam3_request(10, 10, DocRect::EMPTY, "").is_err());
        assert!(sam3_request(0, 10, DocRect::new(0, 0, 1, 1), "").is_err());
        assert!(sam3_request(10, 10, DocRect::new(20, 20, 30, 30), "").is_err());
    }

    #[test]
    fn the_api_key_round_trips_and_a_blank_key_is_cleared() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        assert!(api_key(&app).is_empty());
        save_api_key(&mut app, "  secret-key  ");
        assert_eq!(api_key(&app), "secret-key");
        let plan = {
            app.run("file.new", json!({ "width": 32, "height": 24 })).unwrap();
            app.run("select.rect", json!({ "x": 4, "y": 4, "width": 8, "height": 6 })).unwrap();
            sam_plan(&app).unwrap()
        };
        let call = plan.expect("a saved key plans a SAM 3 call");
        assert!(!call.body.to_string().contains("secret-key"));
        save_api_key(&mut app, "   ");
        assert!(api_key(&app).is_empty());
        assert!(sam_plan(&app).unwrap().is_none());
    }

    fn harness() -> Harness<'static, PhotocraftApp> {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({ "width": 400, "height": 300 })).unwrap();
        app.sync_views();
        let mut h = Harness::builder().with_size(vec2(1000.0, 760.0)).build_ui_state(
            |ui, app: &mut PhotocraftApp| {
                let ctx = ui.ctx().clone();
                if !ctx.fonts(|f| f.families().contains(&egui::FontFamily::Name("medium".into()))) {
                    return;
                }
                egui::CentralPanel::default().show(ui, |ui| crate::canvas::document_area(app, ui));
                show(app, &ctx);
            },
            app,
        );
        PhotocraftApp::setup_context(&h.ctx, crate::theme::ThemeKind::ProMedium);
        h.run_steps(6);
        h
    }

    #[test]
    fn a_selection_shows_the_bar_and_deselect_clears_it() {
        let mut h = harness();
        assert!(h.query_by_label("Deselect").is_none(), "no bar without a selection");
        h.state_mut().run("select.rect", json!({ "x": 80, "y": 60, "width": 120, "height": 70 })).unwrap();
        h.run_steps(4);
        h.get_by_label("Generative Fill").click();
        h.run_steps(3);
        assert!(h.state().session.active().unwrap().doc.selection.is_some(), "opening Generative Fill keeps the selection");
        assert_eq!(h.state().ui.contextual.pop, Popover::Fill);
        h.get_by_label("Deselect").click();
        h.run_steps(3);
        assert!(h.state().session.active().unwrap().doc.selection.is_none());
        assert!(h.query_by_label("Deselect").is_none(), "the bar leaves with the selection");
    }

    #[test]
    fn generate_without_a_key_opens_the_gear_and_a_prompt_waits_for_upload() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        app.run("file.new", json!({ "width": 32, "height": 24 })).unwrap();
        generate(&mut app);
        assert_eq!(app.ui.contextual.pop, Popover::Gear);
        assert!(app.ui.status_error);
        save_api_key(&mut app, "secret-key");
        generate(&mut app);
        assert!(app.ui.status_error, "an empty prompt is refused");
        app.ui.contextual.prompt = "a red balloon".into();
        generate(&mut app);
        assert!(!app.ui.status_error);
        assert!(app.ui.status.contains("uploaded"));
        assert!(!app.ui.status.contains("secret-key"));
    }

    #[test]
    fn the_window_command_hides_the_bar() {
        let mut app = PhotocraftApp::new(photocraft_engine::Session::new(), crate::Services::default());
        let ctx = egui::Context::default();
        assert!(app.ui.panels.contextual_bar);
        crate::menus::invoke(&mut app, &ctx, "window.toggle.contextualTaskBar", json!({})).unwrap();
        assert!(!app.ui.panels.contextual_bar);
        crate::menus::invoke(&mut app, &ctx, "window.toggle.contextualTaskBar", json!({})).unwrap();
        assert!(app.ui.panels.contextual_bar);
    }
}
