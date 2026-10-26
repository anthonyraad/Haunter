use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor,
    egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind},
    EguiState,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::dsp::PlayMode;
use crate::{load_file, GhostpumpParams, Shared};

pub const WIDTH: u32 = 760;
pub const HEIGHT: u32 = 600;

const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
const SUBTLE: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
const OUTLINE: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4a);
const DISC: Color32 = Color32::from_rgb(0xf4, 0xf6, 0xff);
const TRACK: Color32 = Color32::from_rgb(0xb4, 0xb8, 0xd0);
const ARC: Color32 = Color32::from_rgb(0x3e, 0x4e, 0xd8);
const PLAYHEAD: Color32 = Color32::from_rgb(0xd8, 0x5a, 0x3e);

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

pub fn create(params: Arc<GhostpumpParams>, shared: Arc<Shared>, state: Arc<EguiState>) -> Option<Box<dyn Editor>> {
    create_egui_editor(
        state,
        EditorState::default(),
        |context, _| {
            let mut visuals = egui::Visuals::light();
            visuals.panel_fill = Color32::from_rgb(0xec, 0xec, 0xec);
            visuals.window_fill = Color32::from_rgb(0xd6, 0xd6, 0xd6);
            context.set_visuals(visuals);
        },
        move |context, setter, state| {
            draw(context, setter, &params, &shared, state);
            context.request_repaint();
        },
    )
}

/// Builds one frame of the editor and returns the area the controls used.
fn draw(
    context: &egui::Context,
    setter: &ParamSetter,
    params: &Arc<GhostpumpParams>,
    shared: &Arc<Shared>,
    state: &mut EditorState,
) -> Rect {
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
            toggles_and_meter(ui, setter, params, shared, state);
            ui.add_space(8.0);
            knob_row(ui, setter, &[
                ("Rate", &params.rate),
                ("Depth", &params.depth),
                ("Attack", &params.attack),
                ("Release", &params.release),
                ("Mix", &params.mix),
            ]);
            knob_row(ui, setter, &[
                ("HP", &params.high_pass),
                ("LP", &params.low_pass),
                ("Sample Vol", &params.sample_volume),
                ("Blend", &params.blend),
                ("Output", &params.output),
            ]);
            ui.min_rect()
        })
        .inner
}

fn is_audio_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|known| ext.eq_ignore_ascii_case(known)))
}

fn header(ui: &mut egui::Ui, params: &Arc<GhostpumpParams>, shared: &Arc<Shared>, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Ghostpump").size(22.0).strong().color(INK));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let busy = shared.loading.load(Ordering::Acquire) || state.dialog_open.load(Ordering::Acquire);
            let button = egui::Button::new(egui::RichText::new("Load…").size(14.0).color(INK))
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

fn open_file_dialog(params: Arc<GhostpumpParams>, shared: Arc<Shared>, dialog_open: Arc<AtomicBool>) {
    if dialog_open.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(move || {
        let picked = rfd::FileDialog::new()
            .set_title("Load a sample into Ghostpump")
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
    params: &GhostpumpParams,
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

fn handle_param(params: &GhostpumpParams, handle: Handle) -> &FloatParam {
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

fn segment(ui: &mut egui::Ui, selected: bool, text: &str) -> egui::Response {
    let (fill, color) = if selected { (ARC, DISC) } else { (DISC, INK) };
    ui.add(
        egui::Button::new(egui::RichText::new(text).size(13.0).color(color))
            .min_size(egui::vec2(78.0, 26.0))
            .fill(fill)
            .stroke(Stroke::new(1.0_f32, OUTLINE)),
    )
}

fn toggles_and_meter(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &GhostpumpParams,
    shared: &Shared,
    state: &mut EditorState,
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("MODE").size(11.0).color(SUBTLE));
        let mode = params.mode.value();
        for (choice, text) in [(PlayMode::OneShot, "One-shot"), (PlayMode::Loop, "Loop")] {
            if segment(ui, mode == choice, text).clicked() && mode != choice {
                setter.begin_set_parameter(&params.mode);
                setter.set_parameter(&params.mode, choice);
                setter.end_set_parameter(&params.mode);
            }
        }
        ui.add_space(16.0);
        let invert = params.invert.value();
        if segment(ui, invert, "Invert").clicked() {
            setter.begin_set_parameter(&params.invert);
            setter.set_parameter(&params.invert, !invert);
            setter.end_set_parameter(&params.invert);
        }

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
    });
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

trait KnobParam {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str);
}

impl<P: Param> KnobParam for P {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str) {
        draw_knob(ui, setter, self, name);
    }
}

fn draw_knob<P: Param>(ui: &mut egui::Ui, setter: &ParamSetter, param: &P, name: &str) {
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

    let dial = 74.0_f32.min(rect.width() - 8.0);
    let center = Pos2::new(rect.center().x, rect.top() + dial * 0.5 + 2.0);
    let radius = dial * 0.5 - 3.0;
    let angle = ROTARY_START + normalized * (ROTARY_END - ROTARY_START);
    let painter = ui.painter();
    painter.circle_filled(center, radius - 1.0, DISC);
    painter.circle_stroke(center, radius - 1.0, Stroke::new(1.0_f32, OUTLINE));
    painter.line(arc_points(center, radius - 5.0, ROTARY_START, ROTARY_END), Stroke::new(2.0_f32, TRACK));
    if normalized > 0.004 {
        painter.line(arc_points(center, radius - 5.0, ROTARY_START, angle), Stroke::new(2.0_f32, ARC));
    }
    let inner = Pos2::new(center.x + angle.sin() * dial * 0.18, center.y - angle.cos() * dial * 0.18);
    let outer = Pos2::new(center.x + angle.sin() * dial * 0.36, center.y - angle.cos() * dial * 0.36);
    painter.line_segment([inner, outer], Stroke::new(1.5_f32, INK));

    painter.text(
        Pos2::new(rect.center().x, rect.top() + dial + 14.0),
        egui::Align2::CENTER_CENTER,
        param.to_string(),
        egui::FontId::proportional(13.0),
        INK,
    );
    painter.text(
        Pos2::new(rect.center().x, rect.top() + dial + 30.0),
        egui::Align2::CENTER_CENTER,
        name.to_uppercase(),
        egui::FontId::proportional(11.0),
        SUBTLE,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct NoHost;

    impl GuiContext for NoHost {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Vst3
        }
        fn request_resize(&self) -> bool {
            false
        }
        unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {}
        unsafe fn raw_set_parameter_normalized(&self, _param: ParamPtr, _normalized: f32) {}
        unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {}
        fn get_state(&self) -> PluginState {
            PluginState { version: String::new(), params: BTreeMap::new(), fields: BTreeMap::new() }
        }
        fn set_state(&self, _state: PluginState) {}
    }

    fn used_area(with_sample: bool) -> Rect {
        let plugin = crate::Ghostpump::default();
        if with_sample {
            let kick = (0..4_800).map(|n| (n as f32 * 0.01).sin() * (1.0 - n as f32 / 4_800.0)).collect();
            let sample = crate::sample::EmbeddedSample { name: "kick.wav".into(), sample_rate: 48_000.0, channels: vec![kick] };
            crate::install_sample(sample, &plugin.params, &plugin.shared);
            plugin.shared.playhead.store(0.3, Ordering::Relaxed);
            plugin.shared.gain_reduction_db.store(9.0, Ordering::Relaxed);
        }
        let host = NoHost;
        let setter = ParamSetter::new(&host);
        let context = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(WIDTH as f32, HEIGHT as f32))),
            ..Default::default()
        };
        let mut state = EditorState::default();
        let mut used = Rect::NOTHING;
        // The first pass measures text, the second lays out with those sizes.
        for _ in 0..2 {
            let _ = context.run(input.clone(), |context| {
                used = draw(context, &setter, &plugin.params, &plugin.shared, &mut state);
            });
        }
        used
    }

    #[test]
    fn every_control_fits_in_the_window() {
        for with_sample in [false, true] {
            let used = used_area(with_sample);
            assert!(used.max.x <= WIDTH as f32, "too wide: {used:?}");
            assert!(used.max.y <= HEIGHT as f32, "too tall: {used:?}");
            assert!(used.max.y > HEIGHT as f32 * 0.8, "unexpectedly short layout: {used:?}");
        }
    }
}

fn knob_row(ui: &mut egui::Ui, setter: &ParamSetter, knobs: &[(&str, &dyn KnobParam)]) {
    let width = ui.available_width();
    let height = 118.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    for (index, (name, param)) in knobs.iter().enumerate() {
        let center_x = rect.left() + width * (index as f32 + 0.5) / KNOB_COLUMNS;
        let slot = Rect::from_center_size(Pos2::new(center_x, rect.center().y), egui::vec2(120.0, height));
        ui.allocate_new_ui(
            egui::UiBuilder::new()
                .id_salt(*name)
                .max_rect(slot)
                .layout(egui::Layout::top_down(egui::Align::Center)),
            |ui| param.draw(ui, setter, name),
        );
    }
}
