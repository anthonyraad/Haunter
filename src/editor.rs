use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor,
    egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind},
    EguiState,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{load_file, HaunterParams, Shared};

pub const WIDTH: u32 = 760;
pub const HEIGHT: u32 = 600;

const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
const SUBTLE: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
const OUTLINE: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4a);
const DISC: Color32 = Color32::from_rgb(0xf4, 0xf6, 0xff);
const TRACK: Color32 = Color32::from_rgb(0xb4, 0xb8, 0xd0);
const ARC: Color32 = Color32::from_rgb(0x3e, 0x4e, 0xd8);
const PLAYHEAD: Color32 = Color32::from_rgb(0xd8, 0x5a, 0x3e);
// Dream Eater's dark knob, the same as Dropout's The End knob in its Destroy section.
const EATER_DISC: Color32 = Color32::from_rgb(0x6e, 0x6e, 0x6e);
const EATER_OUTLINE: Color32 = Color32::from_rgb(0x2a, 0x2a, 0x2a);
const EATER_TRACK: Color32 = Color32::from_rgb(0x3f, 0x3f, 0x3f);
const EATER_ARC: Color32 = Color32::WHITE;

const ROTARY_START: f32 = std::f32::consts::PI * 1.2;
const ROTARY_END: f32 = std::f32::consts::PI * 2.8;
const KNOB_COLUMNS: f32 = 5.0;
/// The meter's full scale, in dB of reduction.
const METER_RANGE_DB: f32 = 36.0;
const AUDIO_EXTENSIONS: [&str; 5] = ["wav", "wave", "aif", "aiff", "aifc"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    Start,
    End,
}

#[derive(Default)]
struct EditorState {
    meter_db: f32,
    dragging: Option<Handle>,
    dialog_open: Arc<AtomicBool>,
}

pub fn create(params: Arc<HaunterParams>, shared: Arc<Shared>, state: Arc<EguiState>) -> Option<Box<dyn Editor>> {
    create_egui_editor(
        state,
        EditorState::default(),
        |context, _| {
            let mut visuals = egui::Visuals::light();
            visuals.panel_fill = Color32::from_rgb(0xec, 0xec, 0xec);
            visuals.window_fill = Color32::from_rgb(0xd6, 0xd6, 0xd6);
            context.set_visuals(visuals);
            configure(context);
        },
        move |context, setter, state| {
            draw(context, setter, &params, &shared, state);
            context.request_repaint();
        },
    )
}

fn configure(context: &egui::Context) {
    // Selectable labels take clicks, so one overlapping a button would swallow them.
    context.style_mut(|style| style.interaction.selectable_labels = false);
}

/// Where one frame of the editor put things.
#[cfg_attr(not(test), allow(dead_code))]
struct Drawn {
    used: Rect,
    restart: Rect,
    reverse: Rect,
    /// The second row of knobs, which ends with Dream Eater.
    lower_knobs: Rect,
}

/// Builds one frame of the editor.
fn draw(
    context: &egui::Context,
    setter: &ParamSetter,
    params: &Arc<HaunterParams>,
    shared: &Arc<Shared>,
    state: &mut EditorState,
) -> Drawn {
    if shared.reset_region.swap(false, Ordering::AcqRel) {
        for (param, value) in [(&params.start, 0.0), (&params.end, 1.0)] {
            setter.begin_set_parameter(param);
            setter.set_parameter(param, value);
            setter.end_set_parameter(param);
        }
    }
    let dropped = context.input(|input| {
        input.raw.dropped_files.iter().filter_map(|file| file.path.clone()).find(|path| is_audio_file(path))
    });
    if let Some(path) = dropped {
        load_file(path, params.clone(), shared.clone());
    }
    let hovering_files = context.input(|input| !input.raw.hovered_files.is_empty());

    egui::CentralPanel::default()
        .show(context, |ui| {
            header(ui, params, shared, state);
            ui.add_space(8.0);
            waveform(ui, setter, params, shared, state, hovering_files);
            ui.add_space(10.0);
            let [restart, reverse] = toggles_and_meter(ui, setter, params, shared, state);
            ui.add_space(8.0);
            // Without Restart the region just loops, so the grid spacing does nothing.
            let rate_enabled = params.restart.value();
            knob_row(ui, setter, &[
                ("Rate", &params.rate, rate_enabled, Face::Plain),
                ("Depth", &params.depth, true, Face::Plain),
                ("Attack", &params.attack, true, Face::Plain),
                ("Release", &params.release, true, Face::Plain),
                ("Mix", &params.mix, true, Face::Plain),
            ]);
            let dream_level = shared.dream_level.load(Ordering::Relaxed);
            let lower_knobs = knob_row(ui, setter, &[
                ("HP", &params.high_pass, true, Face::Plain),
                ("LP", &params.low_pass, true, Face::Plain),
                ("Sample Vol", &params.sample_volume, true, Face::Plain),
                ("Blend", &params.blend, true, Face::Plain),
                ("Dream Eater", &params.dream_eater, true, Face::Eater { progress: dream_level }),
            ]);
            Drawn { used: ui.min_rect(), restart, reverse, lower_knobs }
        })
        .inner
}

fn is_audio_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|known| ext.eq_ignore_ascii_case(known)))
}

fn header(ui: &mut egui::Ui, params: &Arc<HaunterParams>, shared: &Arc<Shared>, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Haunter").size(22.0).strong().color(INK));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let busy = shared.loading.load(Ordering::Acquire) || state.dialog_open.load(Ordering::Acquire);
            let button = egui::Button::new(egui::RichText::new("Load").size(14.0).color(INK))
                .min_size(egui::vec2(86.0, 26.0))
                .fill(DISC)
                .stroke(Stroke::new(1.0_f32, OUTLINE));
            if ui.add_enabled(!busy, button).clicked() {
                open_file_dialog(params.clone(), shared.clone(), state.dialog_open.clone());
            }
        });
    });
    let status = shared.status.lock().unwrap().clone();
    let line = if !status.is_empty() {
        status
    } else if let Some(overview) = shared.overview.lock().unwrap().as_ref() {
        format!("{} · {:.0} ms", overview.name, overview.seconds * 1000.0)
    } else {
        "Load a kick or any one-shot. Its volume shape ducks this track on the tempo grid.".to_owned()
    };
    ui.label(egui::RichText::new(line).color(SUBTLE));
}

fn open_file_dialog(params: Arc<HaunterParams>, shared: Arc<Shared>, dialog_open: Arc<AtomicBool>) {
    if dialog_open.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let picked = rfd::FileDialog::new()
            .set_title("Load a sample into Haunter")
            .add_filter("WAV or AIFF", &AUDIO_EXTENSIONS)
            .pick_file();
        dialog_open.store(false, Ordering::Release);
        if let Some(path) = picked {
            load_file(path, params, shared);
        }
    });
}

fn waveform(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &HaunterParams,
    shared: &Shared,
    state: &mut EditorState,
    hovering_files: bool,
) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 180.0), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect(rect, CornerRadius::same(6), DISC, Stroke::new(1.0_f32, OUTLINE), StrokeKind::Inside);

    let overview = shared.overview.lock().unwrap().clone();
    let inner = rect.shrink2(egui::vec2(10.0, 14.0));
    let x_of = |fraction: f32| inner.left() + fraction.clamp(0.0, 1.0) * inner.width();
    let fraction_of = |x: f32| ((x - inner.left()) / inner.width()).clamp(0.0, 1.0);

    let Some(overview) = overview else {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop a WAV or AIFF here, or click Load",
            egui::FontId::proportional(15.0),
            SUBTLE,
        );
        drop_overlay(&painter, rect, hovering_files);
        return;
    };

    let start = params.start.modulated_plain_value();
    let end = params.end.modulated_plain_value();
    let (low, high) = (start.min(end), start.max(end));

    // Handle dragging: grab whichever handle is nearer the pointer.
    if response.drag_started() {
        if let Some(pointer) = response.interact_pointer_pos() {
            let to_start = (pointer.x - x_of(start)).abs();
            let to_end = (pointer.x - x_of(end)).abs();
            let handle = if to_start <= to_end { Handle::Start } else { Handle::End };
            setter.begin_set_parameter(handle_param(params, handle));
            state.dragging = Some(handle);
        }
    }
    if let (Some(handle), Some(pointer)) = (state.dragging, response.interact_pointer_pos()) {
        if response.dragged() {
            let gap = 0.002;
            let value = match handle {
                Handle::Start => fraction_of(pointer.x).min(end - gap).max(0.0),
                Handle::End => fraction_of(pointer.x).max(start + gap).min(1.0),
            };
            setter.set_parameter(handle_param(params, handle), value);
        }
    }
    if response.drag_stopped() {
        if let Some(handle) = state.dragging.take() {
            setter.end_set_parameter(handle_param(params, handle));
        }
    }
    if let Some(pointer) = response.hover_pos() {
        let near = (pointer.x - x_of(start)).abs() < 12.0 || (pointer.x - x_of(end)).abs() < 12.0;
        if near || state.dragging.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        }
    }

    // Waveform, brighter inside the region.
    let middle = inner.center().y;
    let half_height = inner.height() * 0.5;
    let peak = overview.columns.iter().fold(1e-6f32, |m, (lo, hi)| m.max(lo.abs()).max(hi.abs()));
    let scale = half_height / peak;
    let pixels = inner.width().max(1.0) as usize;
    for pixel in 0..pixels {
        let fraction = (pixel as f32 + 0.5) / pixels as f32;
        let column = ((fraction * overview.columns.len() as f32) as usize).min(overview.columns.len() - 1);
        let (lo, hi) = overview.columns[column];
        let x = inner.left() + pixel as f32 + 0.5;
        let color = if fraction >= low && fraction <= high { ARC } else { TRACK };
        painter.line_segment(
            [Pos2::new(x, middle - hi * scale - 0.5), Pos2::new(x, middle - lo * scale + 0.5)],
            Stroke::new(1.0_f32, color),
        );
    }
    painter.line_segment(
        [Pos2::new(inner.left(), middle), Pos2::new(inner.right(), middle)],
        Stroke::new(1.0_f32, TRACK.gamma_multiply(0.6)),
    );
    let shade = Color32::from_rgba_unmultiplied(0x60, 0x60, 0x70, 40);
    painter.rect_filled(Rect::from_min_max(rect.min, Pos2::new(x_of(low), rect.bottom())), 0.0, shade);
    painter.rect_filled(Rect::from_min_max(Pos2::new(x_of(high), rect.top()), rect.max), 0.0, shade);

    let playhead = shared.playhead.load(Ordering::Relaxed);
    if playhead >= 0.0 {
        let x = x_of(playhead);
        painter.line_segment([Pos2::new(x, rect.top() + 2.0), Pos2::new(x, rect.bottom() - 2.0)], Stroke::new(1.5_f32, PLAYHEAD));
    }

    for (handle, value, label) in [(Handle::Start, start, "S"), (Handle::End, end, "E")] {
        let x = x_of(value);
        let active = state.dragging == Some(handle);
        let stroke = Stroke::new(if active { 2.5_f32 } else { 1.5 }, INK);
        painter.line_segment([Pos2::new(x, rect.top() + 1.0), Pos2::new(x, rect.bottom() - 1.0)], stroke);
        let tab = Rect::from_center_size(Pos2::new(x, rect.top() + 10.0), egui::vec2(16.0, 16.0));
        painter.rect_filled(tab, CornerRadius::same(3), INK);
        painter.text(tab.center(), egui::Align2::CENTER_CENTER, label, egui::FontId::proportional(11.0), DISC);
        let readout = handle_param(params, handle).to_string();
        let anchor = if handle == Handle::Start { egui::Align2::LEFT_BOTTOM } else { egui::Align2::RIGHT_BOTTOM };
        let offset = if handle == Handle::Start { 5.0 } else { -5.0 };
        painter.text(Pos2::new(x + offset, rect.bottom() - 4.0), anchor, readout, egui::FontId::proportional(11.0), SUBTLE);
    }

    drop_overlay(&painter, rect, hovering_files);
}

fn handle_param(params: &HaunterParams, handle: Handle) -> &FloatParam {
    match handle {
        Handle::Start => &params.start,
        Handle::End => &params.end,
    }
}

fn drop_overlay(painter: &egui::Painter, rect: Rect, hovering_files: bool) {
    if hovering_files {
        painter.rect_filled(rect, CornerRadius::same(6), Color32::from_rgba_unmultiplied(0x3e, 0x4e, 0xd8, 60));
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Drop to load", egui::FontId::proportional(18.0), INK);
    }
}

/// A button that flips `param` and is lit while it is on. Returns where it was drawn.
fn toggle(ui: &mut egui::Ui, setter: &ParamSetter, param: &BoolParam, text: &str, help: &str) -> Rect {
    let on = param.value();
    let (fill, color) = if on { (ARC, DISC) } else { (DISC, INK) };
    let button = egui::Button::new(egui::RichText::new(text).size(13.0).color(color))
        .min_size(egui::vec2(86.0, 26.0))
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, OUTLINE));
    let response = ui.add(button).on_hover_text(help);
    if response.clicked() {
        setter.begin_set_parameter(param);
        setter.set_parameter(param, !on);
        setter.end_set_parameter(param);
    }
    response.rect
}

/// Returns where the Restart and Reverse toggles were drawn.
fn toggles_and_meter(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &HaunterParams,
    shared: &Shared,
    state: &mut EditorState,
) -> [Rect; 2] {
    ui.horizontal(|ui| {
        let restart = toggle(
            ui,
            setter,
            &params.restart,
            "Restart",
            "On: the region starts again on every Rate step.\nOff: it loops continuously and Rate is ignored.",
        );
        let reverse = toggle(ui, setter, &params.reverse, "Reverse", "Plays the region backwards, from E to S.");

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let latest = shared.gain_reduction_db.swap(0.0, Ordering::Relaxed);
            state.meter_db = latest.max(state.meter_db - 0.6).max(0.0);
            let (rect, _) = ui.allocate_exact_size(egui::vec2(220.0, 18.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect(rect, CornerRadius::same(3), DISC, Stroke::new(1.0_f32, OUTLINE), StrokeKind::Inside);
            let fill = (state.meter_db / METER_RANGE_DB).clamp(0.0, 1.0);
            if fill > 0.0 {
                let bar = Rect::from_min_max(rect.min, Pos2::new(rect.left() + rect.width() * fill, rect.bottom()))
                    .shrink(2.0);
                painter.rect_filled(bar, CornerRadius::same(2), ARC);
            }
            for db in [6.0, 12.0, 24.0] {
                let x = rect.left() + rect.width() * db / METER_RANGE_DB;
                painter.line_segment([Pos2::new(x, rect.bottom() - 4.0), Pos2::new(x, rect.bottom())], Stroke::new(1.0_f32, OUTLINE));
            }
            ui.label(egui::RichText::new(format!("{:>5.1} dB", -state.meter_db)).monospace().color(INK));
            ui.label(egui::RichText::new("GAIN REDUCTION").size(11.0).color(SUBTLE));
        });
        [restart, reverse]
    })
    .inner
}

fn arc_points(center: Pos2, radius: f32, start: f32, end: f32) -> Vec<Pos2> {
    let steps = (((end - start).abs() / 0.09).ceil() as i32).max(2);
    (0..=steps)
        .map(|step| {
            let angle = start + (end - start) * (step as f32 / steps as f32);
            Pos2::new(center.x + angle.sin() * radius, center.y - angle.cos() * radius)
        })
        .collect()
}

/// How a knob is painted.
#[derive(Clone, Copy, PartialEq)]
enum Face {
    Plain,
    /// Dropout's Destroy treatment: a dark disc with a white arc and pointer, and `progress`
    /// (0..1, the sample's live sweep) filling the white arc in blue from the start.
    Eater { progress: f32 },
}

trait KnobParam {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str, face: Face);
}

impl<P: Param> KnobParam for P {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str, face: Face) {
        draw_knob(ui, setter, self, name, face);
    }
}

fn draw_knob<P: Param>(ui: &mut egui::Ui, setter: &ParamSetter, param: &P, name: &str, face: Face) {
    let width = ui.available_width().max(88.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 112.0), egui::Sense::click_and_drag());
    let normalized = param.modulated_normalized_value();
    let memory = ui.id().with(name);

    if response.drag_started() {
        setter.begin_set_parameter(param);
        ui.memory_mut(|mem| {
            mem.data.insert_temp(memory.with("start"), normalized);
            mem.data.insert_temp(memory.with("drag"), 0.0f32);
        });
    }
    if response.dragged() {
        let (start, mut dragged) = ui.memory(|mem| {
            (
                mem.data.get_temp::<f32>(memory.with("start")).unwrap_or(normalized),
                mem.data.get_temp::<f32>(memory.with("drag")).unwrap_or(0.0),
            )
        });
        dragged += -response.drag_delta().y;
        ui.memory_mut(|mem| mem.data.insert_temp(memory.with("drag"), dragged));
        let fine = ui.input(|input| input.modifiers.shift);
        let scale = if fine { 0.0015 } else { 1.0 / 140.0 };
        let next = (start + dragged * scale).clamp(0.0, 1.0);
        setter.set_parameter(param, param.preview_plain(next));
    }
    if response.double_clicked() || (response.clicked() && ui.input(|input| input.modifiers.command)) {
        setter.begin_set_parameter(param);
        setter.set_parameter(param, param.default_plain_value());
        setter.end_set_parameter(param);
    }
    if response.drag_stopped() {
        setter.end_set_parameter(param);
    }

    let enabled = ui.is_enabled();
    let tint = |color: Color32| if enabled { color } else { color.gamma_multiply(0.35) };
    let dial = 74.0_f32.min(rect.width() - 8.0);
    let center = Pos2::new(rect.center().x, rect.top() + dial * 0.5 + 2.0);
    let radius = dial * 0.5 - 3.0;
    let angle = ROTARY_START + normalized * (ROTARY_END - ROTARY_START);
    let (disc, outline, track, arc, pointer, pointer_width) = match face {
        Face::Plain => (DISC, OUTLINE, TRACK, ARC, INK, 1.5_f32),
        Face::Eater { .. } => (EATER_DISC, EATER_OUTLINE, EATER_TRACK, EATER_ARC, EATER_ARC, 2.2),
    };
    let painter = ui.painter();
    painter.circle_filled(center, radius - 1.0, tint(disc));
    painter.circle_stroke(center, radius - 1.0, Stroke::new(1.0_f32, tint(outline)));
    painter.line(arc_points(center, radius - 5.0, ROTARY_START, ROTARY_END), Stroke::new(2.0_f32, tint(track)));
    if normalized > 0.004 {
        painter.line(arc_points(center, radius - 5.0, ROTARY_START, angle), Stroke::new(2.0_f32, tint(arc)));
    }
    if let Face::Eater { progress } = face {
        let progress = progress.clamp(0.0, 1.0);
        if progress > 0.004 && angle > ROTARY_START + 0.004 {
            let filled = ROTARY_START + progress * (angle - ROTARY_START);
            painter.line(arc_points(center, radius - 5.0, ROTARY_START, filled), Stroke::new(2.0_f32, tint(ARC)));
        }
    }
    let inner = Pos2::new(center.x + angle.sin() * dial * 0.18, center.y - angle.cos() * dial * 0.18);
    let outer = Pos2::new(center.x + angle.sin() * dial * 0.36, center.y - angle.cos() * dial * 0.36);
    painter.line_segment([inner, outer], Stroke::new(pointer_width, tint(pointer)));

    painter.text(
        Pos2::new(rect.center().x, rect.top() + dial + 14.0),
        egui::Align2::CENTER_CENTER,
        param.to_string(),
        egui::FontId::proportional(13.0),
        tint(INK),
    );
    painter.text(
        Pos2::new(rect.center().x, rect.top() + dial + 30.0),
        egui::Align2::CENTER_CENTER,
        name.to_uppercase(),
        egui::FontId::proportional(11.0),
        tint(SUBTLE),
    );
}

/// Draws a row of knobs, each with whether it can be used and how it looks. Returns the row's area.
fn knob_row(ui: &mut egui::Ui, setter: &ParamSetter, knobs: &[(&str, &dyn KnobParam, bool, Face)]) -> Rect {
    let width = ui.available_width();
    let height = 118.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    for (index, (name, param, enabled, face)) in knobs.iter().enumerate() {
        let center_x = rect.left() + width * (index as f32 + 0.5) / KNOB_COLUMNS;
        let slot = Rect::from_center_size(Pos2::new(center_x, rect.center().y), egui::vec2(120.0, height));
        let mut builder = egui::UiBuilder::new()
            .id_salt(*name)
            .max_rect(slot)
            .layout(egui::Layout::top_down(egui::Align::Center));
        if !enabled {
            builder = builder.disabled();
        }
        ui.allocate_new_ui(builder, |ui| param.draw(ui, setter, name, *face));
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Records the parameter changes the editor asks the host for.
    #[derive(Default)]
    struct Host {
        edits: Mutex<Vec<(ParamPtr, f32)>>,
    }

    impl GuiContext for Host {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Vst3
        }
        fn request_resize(&self) -> bool {
            false
        }
        unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {}
        unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
            self.edits.lock().unwrap().push((param, normalized));
        }
        unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {}
        fn get_state(&self) -> PluginState {
            PluginState { version: String::new(), params: BTreeMap::new(), fields: BTreeMap::new() }
        }
        fn set_state(&self, _state: PluginState) {}
    }

    /// A headless egui window the size of the editor.
    struct Screen {
        host: Host,
        context: egui::Context,
        time: f64,
        /// Everything painted in the last frame.
        shapes: Vec<egui::Shape>,
    }

    impl Screen {
        fn new() -> Self {
            let context = egui::Context::default();
            configure(&context);
            Screen { host: Host::default(), context, time: 0.0, shapes: Vec::new() }
        }

        fn frame<R>(&mut self, events: Vec<egui::Event>, mut ui: impl FnMut(&egui::Context, &ParamSetter) -> R) -> R {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(WIDTH as f32, HEIGHT as f32))),
                time: Some(self.time),
                events,
                ..Default::default()
            };
            let setter = ParamSetter::new(&self.host);
            let mut result = None;
            let output = self.context.run(input, |context| result = Some(ui(context, &setter)));
            fn flatten(shape: egui::Shape, into: &mut Vec<egui::Shape>) {
                match shape {
                    egui::Shape::Vec(shapes) => shapes.into_iter().for_each(|shape| flatten(shape, into)),
                    shape => into.push(shape),
                }
            }
            self.shapes.clear();
            for clipped in output.shapes {
                flatten(clipped.shape, &mut self.shapes);
            }
            result.unwrap()
        }

        /// Centres of the filled discs painted in `color`.
        fn discs(&self, color: Color32) -> Vec<Pos2> {
            self.shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Circle(circle) if circle.fill == color => Some(circle.center),
                    _ => None,
                })
                .collect()
        }

        fn take_edits(&self) -> Vec<(ParamPtr, f32)> {
            std::mem::take(&mut *self.host.edits.lock().unwrap())
        }
    }

    fn moved(pos: Pos2) -> Vec<egui::Event> {
        vec![egui::Event::PointerMoved(pos)]
    }

    fn button(pos: Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Default::default() }]
    }

    fn click(pos: Pos2) -> Vec<Vec<egui::Event>> {
        vec![moved(pos), button(pos, true), button(pos, false)]
    }

    struct Editor {
        plugin: crate::Haunter,
        screen: Screen,
        state: EditorState,
    }

    impl Editor {
        fn new(with_sample: bool) -> Self {
            let plugin = crate::Haunter::default();
            if with_sample {
                let kick = (0..4_800).map(|n| (n as f32 * 0.01).sin() * (1.0 - n as f32 / 4_800.0)).collect();
                let sample =
                    crate::sample::EmbeddedSample { name: "kick.wav".into(), sample_rate: 48_000.0, channels: vec![kick] };
                crate::install_sample(sample, &plugin.params, &plugin.shared);
                plugin.shared.playhead.store(0.3, Ordering::Relaxed);
                plugin.shared.gain_reduction_db.store(9.0, Ordering::Relaxed);
            }
            Editor { plugin, screen: Screen::new(), state: EditorState::default() }
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> Drawn {
            let Editor { plugin, screen, state } = self;
            screen.frame(events, |context, setter| draw(context, setter, &plugin.params, &plugin.shared, state))
        }

        /// The first pass measures text, the second lays out with those sizes.
        fn settle(&mut self) -> Drawn {
            self.frame(Vec::new());
            self.frame(Vec::new())
        }

        fn play(&mut self, frames: Vec<Vec<egui::Event>>) {
            for events in frames {
                self.frame(events);
            }
        }
    }

    #[test]
    fn every_control_fits_in_the_window() {
        for with_sample in [false, true] {
            let used = Editor::new(with_sample).settle().used;
            assert!(used.max.x <= WIDTH as f32, "too wide: {used:?}");
            assert!(used.max.y <= HEIGHT as f32, "too tall: {used:?}");
            assert!(used.max.y > HEIGHT as f32 * 0.8, "unexpectedly short layout: {used:?}");
        }
    }

    #[test]
    fn restart_and_reverse_toggle_when_clicked_anywhere_on_them() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        editor.screen.take_edits();
        assert!(drawn.restart.width() >= 80.0 && drawn.reverse.width() >= 80.0);
        assert!(!drawn.restart.intersects(drawn.reverse));
        let restart = editor.plugin.params.restart.as_ptr();
        let reverse = editor.plugin.params.reverse.as_ptr();

        for rect in [drawn.reverse, drawn.reverse.shrink(3.0)] {
            for pos in [rect.center(), rect.left_top(), rect.right_bottom()] {
                editor.play(click(pos));
                assert_eq!(editor.screen.take_edits(), vec![(reverse, 1.0)], "Reverse clicked at {pos:?}");
            }
        }
        editor.play(click(drawn.restart.center()));
        assert_eq!(editor.screen.take_edits(), vec![(restart, 0.0)], "Restart starts on, so a click turns it off");

        // A quick click where the press and release land in the same frame.
        let pos = drawn.reverse.center();
        editor.frame(moved(pos));
        editor.frame([button(pos, true), button(pos, false)].concat());
        assert_eq!(editor.screen.take_edits(), vec![(reverse, 1.0)]);

        editor.play(click(Pos2::new(drawn.reverse.right() + 20.0, drawn.reverse.center().y)));
        assert!(editor.screen.take_edits().is_empty(), "the space beside the toggles does nothing");
    }

    #[test]
    fn a_disabled_knob_ignores_drags_and_double_clicks() {
        let plugin = crate::Haunter::default();
        let rate = plugin.params.rate.as_ptr();
        for enabled in [true, false] {
            let mut screen = Screen::new();
            let row = |screen: &mut Screen, events| {
                screen.frame(events, |context, setter| {
                    egui::CentralPanel::default()
                        .show(context, |ui| knob_row(ui, setter, &[("Rate", &plugin.params.rate, enabled, Face::Plain)]))
                        .inner
                })
            };
            let rect = row(&mut screen, Vec::new());
            let knob = Pos2::new(rect.left() + rect.width() * 0.5 / KNOB_COLUMNS, rect.top() + 40.0);
            for events in [moved(knob), button(knob, true), moved(knob - egui::vec2(0.0, 20.0)), moved(knob - egui::vec2(0.0, 40.0))] {
                row(&mut screen, events);
            }
            row(&mut screen, button(knob - egui::vec2(0.0, 40.0), false));
            for events in [click(knob), click(knob)].concat() {
                row(&mut screen, events);
            }
            let edits = screen.take_edits();
            if enabled {
                assert!(edits.iter().any(|&(param, _)| param == rate), "an enabled knob turns");
            } else {
                assert!(edits.is_empty(), "a disabled knob does nothing: {edits:?}");
            }
        }
    }

    #[test]
    fn dream_eater_sits_where_output_was_with_the_destroy_look() {
        let mut editor = Editor::new(true);
        let row = editor.settle().lower_knobs;
        let last_column = row.left() + row.width() * 4.5 / KNOB_COLUMNS;

        let dark = editor.screen.discs(EATER_DISC);
        assert_eq!(dark.len(), 1, "only Dream Eater is dark: {dark:?}");
        assert!((dark[0].x - last_column).abs() < 1.0 && row.contains(dark[0]), "last knob of the lower row");
        assert_eq!(editor.screen.discs(DISC).len(), 9, "the other knobs keep their light face");
        let white_pointer = editor.screen.shapes.iter().any(|shape| {
            matches!(shape, egui::Shape::LineSegment { stroke, .. } if stroke.color == EATER_ARC && stroke.width == 2.2)
        });
        assert!(white_pointer, "a thicker white pointer");
        let texts: Vec<String> = editor
            .screen
            .shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Text(text) if (text.pos.x - last_column).abs() < 60.0 && row.contains(text.pos) => {
                    Some(text.galley.text().to_owned())
                }
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"Off".to_owned()) && texts.contains(&"DREAM EATER".to_owned()), "{texts:?}");

        editor.screen.take_edits();
        let knob = Pos2::new(last_column, row.top() + 40.0);
        let up = |by: f32| knob - egui::vec2(0.0, by);
        editor.play(vec![moved(knob), button(knob, true), moved(up(20.0)), moved(up(40.0)), button(up(40.0), false)]);
        let dream = editor.plugin.params.dream_eater.as_ptr();
        let edits = editor.screen.take_edits();
        assert!(edits.iter().any(|&(param, value)| param == dream && value > 0.0), "{edits:?}");
    }
}
