use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor,
    egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind},
    EguiState,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::preset::{self, Choice};
use crate::{apply_preset, load_file, read_preset, save_preset, HaunterParams, Shared};

pub const WIDTH: u32 = 760;
pub const HEIGHT: u32 = 600;

const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
const SUBTLE: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
const OUTLINE: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4a);
const DISC: Color32 = Color32::from_rgb(0xf4, 0xf6, 0xff);
const TRACK: Color32 = Color32::from_rgb(0xb4, 0xb8, 0xd0);
const ARC: Color32 = Color32::from_rgb(0x3e, 0x4e, 0xd8);
const PLAYHEAD: Color32 = Color32::from_rgb(0xd8, 0x5a, 0x3e);
/// The Rate readout while Restart is off.
const MUTED: Color32 = Color32::from_rgb(0x8c, 0x8c, 0x8c);
const ERROR: Color32 = Color32::from_rgb(0xb0, 0x30, 0x20);
const INFINITY: &str = "\u{221E}";
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
    /// The saved presets, read from the folder each time the preset menu opens.
    user_presets: Vec<String>,
    menu_open: bool,
    /// The Save prompt's name while the prompt is open.
    naming: Option<String>,
    /// Puts the cursor in the name field on the next frame.
    focus_name: bool,
    /// Why the last save failed, shown until the prompt closes.
    save_error: Option<String>,
    /// `Shared::processed_blocks` when a preset's parameters were sent to the host.
    preset_sent_at: Option<u64>,
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
    /// The preset menu and the Save button, or nothing while the Save prompt is open.
    preset_menu: Rect,
    save: Rect,
    restart: Rect,
    reverse: Rect,
    /// The first row of knobs, which starts with Rate.
    upper_knobs: Rect,
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
            let [preset_menu, save] = header(ui, setter, params, shared, state);
            ui.add_space(8.0);
            waveform(ui, setter, params, shared, state, hovering_files);
            ui.add_space(10.0);
            let ([restart, reverse], restart_on) = toggles_and_meter(ui, setter, params, shared, state);
            ui.add_space(8.0);
            // Without Restart the region just loops, so the grid spacing does nothing.
            let upper_knobs = knob_row(ui, setter, &[
                ("Rate", &params.rate, restart_on, Face::Plain),
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
            Drawn { used: ui.min_rect(), preset_menu, save, restart, reverse, upper_knobs, lower_knobs }
        })
        .inner
}

fn is_audio_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|known| ext.eq_ignore_ascii_case(known)))
}

/// The title, then the preset menu, Save, and Load, or the Save prompt in their place. Returns
/// where the preset menu and Save were drawn.
fn header(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &Arc<HaunterParams>,
    shared: &Arc<Shared>,
    state: &mut EditorState,
) -> [Rect; 2] {
    let mut drawn = [Rect::NOTHING; 2];
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Haunter").size(22.0).strong().color(INK));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if state.naming.is_some() {
                naming_prompt(ui, params, shared, state);
                return;
            }
            let busy = shared.loading.load(Ordering::Acquire) || state.dialog_open.load(Ordering::Acquire);
            if ui.add_enabled(!busy, header_button("Load", 86.0)).clicked() {
                open_file_dialog(params.clone(), shared.clone(), state.dialog_open.clone());
            }
            let save = ui
                .add_enabled(!busy, header_button("Save", 64.0))
                .on_hover_text("Saves the sample and every setting as a preset.");
            if save.clicked() {
                let name = match params.preset.get().choice {
                    Choice::User(name) => name,
                    _ => String::new(),
                };
                state.naming = Some(name);
                state.focus_name = true;
                state.save_error = None;
            }
            drawn = [preset_menu(ui, setter, params, shared, state, !busy), save.rect];
        });
    });
    let (line, color) = if let Some(error) = &state.save_error {
        (error.clone(), ERROR)
    } else {
        let status = shared.status.lock().unwrap().clone();
        let line = if !status.is_empty() {
            status
        } else if let Some(overview) = shared.overview.lock().unwrap().as_ref() {
            format!("{} · {:.0} ms", overview.name, overview.seconds * 1000.0)
        } else {
            "Load a kick or any one-shot. Its volume shape ducks this track on the tempo grid.".to_owned()
        };
        (line, SUBTLE)
    };
    ui.label(egui::RichText::new(line).color(color));
    drawn
}

fn header_button(text: &str, width: f32) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).size(14.0).color(INK))
        .min_size(egui::vec2(width, 26.0))
        .fill(DISC)
        .stroke(Stroke::new(1.0_f32, OUTLINE))
}

/// The preset's name, with a `*` once anything differs from what the preset set.
fn preset_label(params: &HaunterParams, shared: &Shared, state: &EditorState) -> String {
    let selection = params.preset.get();
    let blocks = shared.processed_blocks.load(Ordering::Relaxed);
    let pending = state.preset_sent_at.is_some_and(|at| blocks < at + HOST_GRACE_BLOCKS);
    let modified = selection.is_modified(params, params.sample.fingerprint(), pending);
    format!("{}{}", selection.choice.name(), if modified { "*" } else { "" })
}

/// The preset menu: `--`, the factory presets, then the user's. Returns where it was drawn.
fn preset_menu(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &HaunterParams,
    shared: &Shared,
    state: &mut EditorState,
    enabled: bool,
) -> Rect {
    let selected = params.preset.get().choice;
    let label = preset_label(params, shared, state);
    let mut picked = None;
    let menu = ui
        .add_enabled_ui(enabled, |ui| {
            ui.spacing_mut().interact_size.y = 26.0;
            let widgets = &mut ui.visuals_mut().widgets;
            for look in [&mut widgets.inactive, &mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
                look.weak_bg_fill = DISC;
                look.bg_stroke = Stroke::new(1.0_f32, OUTLINE);
            }
            egui::ComboBox::from_id_salt("preset")
                .width(190.0)
                .truncate()
                .selected_text(egui::RichText::new(label).size(14.0).color(INK))
                .show_ui(ui, |ui| {
                    if !state.menu_open {
                        state.user_presets = shared.user_presets.list();
                    }
                    let mut item = |ui: &mut egui::Ui, choice: Choice| {
                        if ui.selectable_label(choice == selected, choice.name()).clicked() {
                            picked = Some(choice);
                        }
                    };
                    item(ui, Choice::None);
                    for name in preset::factory_names() {
                        item(ui, Choice::Factory(name.to_owned()));
                    }
                    if !state.user_presets.is_empty() {
                        ui.separator();
                    }
                    for name in &state.user_presets {
                        item(ui, Choice::User(name.clone()));
                    }
                })
        })
        .inner;
    state.menu_open = menu.inner.is_some();
    if let Some(choice) = picked {
        select_preset(choice, setter, params, shared, state);
    }
    menu.response.rect
}

/// Loads a preset. Choosing the selected one again puts it back the way it was saved.
fn select_preset(choice: Choice, setter: &ParamSetter, params: &HaunterParams, shared: &Shared, state: &mut EditorState) {
    match read_preset(&choice, shared) {
        Ok(preset) => {
            apply_preset(choice, preset, setter, params, shared);
            state.preset_sent_at = Some(shared.processed_blocks.load(Ordering::Relaxed));
        }
        Err(error) => shared.set_status(error),
    }
}

/// A name field with OK and Cancel, in place of the preset controls. Enter saves and Escape
/// cancels.
fn naming_prompt(ui: &mut egui::Ui, params: &HaunterParams, shared: &Shared, state: &mut EditorState) {
    let cancel = ui.add(header_button("Cancel", 72.0));
    let ok = ui.add(header_button("OK", 56.0));
    let Some(name) = state.naming.as_mut() else {
        return;
    };
    let field = ui.add(
        egui::TextEdit::singleline(name)
            .desired_width(190.0)
            .char_limit(64)
            .font(egui::FontId::proportional(14.0))
            .margin(egui::vec2(6.0, 4.0))
            .hint_text("Preset name"),
    );
    if std::mem::take(&mut state.focus_name) {
        field.request_focus();
    }
    ui.label(egui::RichText::new("Save as").color(SUBTLE));
    let enter = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
    let escape = ui.input(|input| input.key_pressed(egui::Key::Escape));
    if cancel.clicked() || escape {
        state.naming = None;
        state.save_error = None;
    } else if ok.clicked() || enter {
        match save_preset(name, params, shared) {
            Ok(_) => {
                state.naming = None;
                state.save_error = None;
                state.preset_sent_at = None;
            }
            Err(error) => {
                state.save_error = Some(error);
                state.focus_name = true;
            }
        }
    }
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

/// Blocks the host gets to apply an edit from the editor before the editor shows the parameter
/// again, after a toggle or a preset.
const HOST_GRACE_BLOCKS: u64 = 3;

/// A toggle edit the host has been sent but has not applied to the parameter yet.
#[derive(Clone, Copy)]
struct Toggling {
    target: bool,
    /// The mouse button is still down, so the edit is still open.
    held: bool,
    /// `Shared::processed_blocks` when the edit was closed.
    closed_at: u64,
}

/// A button that flips `param` and is lit while it is on. Returns where it was drawn and whether
/// it shows on.
///
/// While the host is processing, nih-plug leaves the parameter alone and waits for the host to send
/// the edit back in a later block. So the edit opens on the press and is sent every frame until the
/// release, the way a knob drag is, and the button shows the new state until the host applies it.
fn toggle(ui: &mut egui::Ui, setter: &ParamSetter, param: &BoolParam, shared: &Shared, text: &str, help: &str) -> (Rect, bool) {
    let memory = ui.make_persistent_id(text);
    let blocks = shared.processed_blocks.load(Ordering::Relaxed);
    let mut toggling = ui.memory(|mem| mem.data.get_temp::<Toggling>(memory)).filter(|edit| {
        edit.held || (param.value() != edit.target && blocks < edit.closed_at + HOST_GRACE_BLOCKS)
    });
    let on = toggling.map_or(param.value(), |edit| edit.target);

    let (fill, color) = if on { (ARC, DISC) } else { (DISC, INK) };
    let button = egui::Button::new(egui::RichText::new(text).size(13.0).color(color))
        .min_size(egui::vec2(86.0, 26.0))
        .fill(fill)
        .stroke(Stroke::new(1.0_f32, OUTLINE));
    let response = ui.add(button).on_hover_text(help);
    let pressed = response.is_pointer_button_down_on();

    match toggling {
        Some(edit) if edit.held => {
            setter.set_parameter(param, edit.target);
            if !pressed {
                setter.end_set_parameter(param);
                toggling = Some(Toggling { held: false, closed_at: blocks, ..edit });
            }
        }
        // A press and release inside one frame only shows up as the click.
        _ if pressed || response.clicked() => {
            let target = !on;
            setter.begin_set_parameter(param);
            setter.set_parameter(param, target);
            if !pressed {
                setter.end_set_parameter(param);
            }
            toggling = Some(Toggling { target, held: pressed, closed_at: blocks });
        }
        _ => {}
    }
    ui.memory_mut(|mem| match toggling {
        Some(edit) => mem.data.insert_temp(memory, edit),
        None => mem.data.remove::<Toggling>(memory),
    });
    (response.rect, toggling.map_or(param.value(), |edit| edit.target))
}

/// Returns where the Restart and Reverse toggles were drawn, and whether Restart shows on.
fn toggles_and_meter(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &HaunterParams,
    shared: &Shared,
    state: &mut EditorState,
) -> ([Rect; 2], bool) {
    ui.horizontal(|ui| {
        let (restart, restart_on) = toggle(
            ui,
            setter,
            &params.restart,
            shared,
            "Restart",
            "On: the region starts again on every Rate step.\nOff: it loops continuously and Rate is ignored.",
        );
        let (reverse, _) =
            toggle(ui, setter, &params.reverse, shared, "Reverse", "Plays the region backwards, from E to S.");

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
        ([restart, reverse], restart_on)
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
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str, turnable: bool, face: Face);
}

impl<P: Param> KnobParam for P {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str, turnable: bool, face: Face) {
        draw_knob(ui, setter, self, name, turnable, face);
    }
}

/// A knob that isn't `turnable` looks the same but takes no input, and reads a grey infinity.
fn draw_knob<P: Param>(ui: &mut egui::Ui, setter: &ParamSetter, param: &P, name: &str, turnable: bool, face: Face) {
    let width = ui.available_width().max(88.0);
    let sense = if turnable { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 112.0), sense);
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
    let (disc, outline, track, arc, pointer, pointer_width) = match face {
        Face::Plain => (DISC, OUTLINE, TRACK, ARC, INK, 1.5_f32),
        Face::Eater { .. } => (EATER_DISC, EATER_OUTLINE, EATER_TRACK, EATER_ARC, EATER_ARC, 2.2),
    };
    let painter = ui.painter();
    painter.circle_filled(center, radius - 1.0, disc);
    painter.circle_stroke(center, radius - 1.0, Stroke::new(1.0_f32, outline));
    painter.line(arc_points(center, radius - 5.0, ROTARY_START, ROTARY_END), Stroke::new(2.0_f32, track));
    if normalized > 0.004 {
        painter.line(arc_points(center, radius - 5.0, ROTARY_START, angle), Stroke::new(2.0_f32, arc));
    }
    if let Face::Eater { progress } = face {
        let progress = progress.clamp(0.0, 1.0);
        if progress > 0.004 && angle > ROTARY_START + 0.004 {
            let filled = ROTARY_START + progress * (angle - ROTARY_START);
            painter.line(arc_points(center, radius - 5.0, ROTARY_START, filled), Stroke::new(2.0_f32, ARC));
        }
    }
    let inner = Pos2::new(center.x + angle.sin() * dial * 0.18, center.y - angle.cos() * dial * 0.18);
    let outer = Pos2::new(center.x + angle.sin() * dial * 0.36, center.y - angle.cos() * dial * 0.36);
    painter.line_segment([inner, outer], Stroke::new(pointer_width, pointer));

    let readout = Pos2::new(rect.center().x, rect.top() + dial + 14.0);
    if turnable {
        painter.text(readout, egui::Align2::CENTER_CENTER, param.to_string(), egui::FontId::proportional(13.0), INK);
    } else {
        draw_infinity(painter, readout, MUTED);
    }
    painter.text(
        Pos2::new(rect.center().x, rect.top() + dial + 30.0),
        egui::Align2::CENTER_CENTER,
        name.to_uppercase(),
        egui::FontId::proportional(11.0),
        SUBTLE,
    );
}

/// An infinity sign centred on `center`, from the font if it has one and drawn otherwise.
fn draw_infinity(painter: &egui::Painter, center: Pos2, color: Color32) {
    let font = egui::FontId::proportional(18.0);
    if painter.ctx().fonts(|fonts| fonts.has_glyphs(&font, INFINITY)) {
        painter.text(center, egui::Align2::CENTER_CENTER, INFINITY, font, color);
        return;
    }
    // A lemniscate of Gerono stretched sideways.
    let points = (0..=48)
        .map(|step| {
            let t = step as f32 / 48.0 * std::f32::consts::TAU;
            Pos2::new(center.x + 8.0 * t.sin(), center.y + 4.0 * (2.0 * t).sin())
        })
        .collect();
    painter.add(egui::Shape::line(points, Stroke::new(1.6_f32, color)));
}

/// Draws a row of knobs, each with whether it can be turned and how it looks. Returns the row's
/// area.
fn knob_row(ui: &mut egui::Ui, setter: &ParamSetter, knobs: &[(&str, &dyn KnobParam, bool, Face)]) -> Rect {
    let width = ui.available_width();
    let height = 118.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    for (index, (name, param, turnable, face)) in knobs.iter().enumerate() {
        let center_x = rect.left() + width * (index as f32 + 0.5) / KNOB_COLUMNS;
        let slot = Rect::from_center_size(Pos2::new(center_x, rect.center().y), egui::vec2(120.0, height));
        let builder = egui::UiBuilder::new()
            .id_salt(*name)
            .max_rect(slot)
            .layout(egui::Layout::top_down(egui::Align::Center));
        ui.allocate_new_ui(builder, |ui| param.draw(ui, setter, name, *turnable, *face));
    }
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// One call the editor made to the host.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Edit {
        Begin(ParamPtr),
        Set(ParamPtr, f32),
        End(ParamPtr),
    }

    /// Records what the editor asks of the host. Like nih-plug's VST3 wrapper while the host is
    /// processing, it leaves the parameters alone: they would only change once the host sent the
    /// edit back in a later block.
    #[derive(Default)]
    struct Host {
        edits: Mutex<Vec<Edit>>,
    }

    impl GuiContext for Host {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Vst3
        }
        fn request_resize(&self) -> bool {
            false
        }
        unsafe fn raw_begin_set_parameter(&self, param: ParamPtr) {
            self.edits.lock().unwrap().push(Edit::Begin(param));
        }
        unsafe fn raw_set_parameter_normalized(&self, param: ParamPtr, normalized: f32) {
            self.edits.lock().unwrap().push(Edit::Set(param, normalized));
        }
        unsafe fn raw_end_set_parameter(&self, param: ParamPtr) {
            self.edits.lock().unwrap().push(Edit::End(param));
        }
        fn get_state(&self) -> PluginState {
            PluginState { version: String::new(), params: BTreeMap::new(), fields: BTreeMap::new() }
        }
        fn set_state(&self, _state: PluginState) {}
    }

    /// The editor's egui context, driven the way `vendor/egui-baseview` drives it in a host:
    /// baseview's cursor and button events become egui pointer events, a button event takes the
    /// last cursor position and is dropped without one, and each tick of baseview's 15 ms frame
    /// timer runs a single `begin_pass`/`end_pass` with the clock and the screen rect.
    struct Screen {
        host: Host,
        context: egui::Context,
        input: egui::RawInput,
        cursor: Option<Pos2>,
        time: f64,
        /// Everything painted in the last frame.
        shapes: Vec<egui::Shape>,
    }

    impl Screen {
        fn new(pixels_per_point: f32) -> Self {
            let context = egui::Context::default();
            configure(&context);
            let mut input = egui::RawInput { max_texture_side: Some(8192), ..Default::default() };
            let viewport = egui::ViewportInfo {
                native_pixels_per_point: Some(pixels_per_point),
                focused: Some(true),
                ..Default::default()
            };
            input.viewports.insert(egui::ViewportId::ROOT, viewport);
            Screen { host: Host::default(), context, input, cursor: None, time: 0.0, shapes: Vec::new() }
        }

        /// baseview's `CursorMoved`.
        fn cursor_moved(&mut self, pos: Pos2) {
            self.cursor = Some(pos);
            self.input.events.push(egui::Event::PointerMoved(pos));
        }

        /// baseview's `ButtonPressed` or `ButtonReleased` for the left button.
        fn left_button(&mut self, pressed: bool) {
            if let Some(pos) = self.cursor {
                let modifiers = self.input.modifiers;
                let button = egui::PointerButton::Primary;
                self.input.events.push(egui::Event::PointerButton { pos, button, pressed, modifiers });
            }
        }

        /// One tick of baseview's frame timer.
        fn frame<R>(&mut self, ui: impl FnOnce(&egui::Context, &ParamSetter) -> R) -> R {
            self.time += 0.015;
            self.input.time = Some(self.time);
            self.input.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(WIDTH as f32, HEIGHT as f32)));
            self.context.begin_pass(self.input.take());
            let setter = ParamSetter::new(&self.host);
            let result = ui(&self.context, &setter);
            let output = self.context.end_pass();
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
            result
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

        /// Whether a button lit in the "on" colour covers the middle of `rect`.
        fn lit(&self, rect: Rect) -> bool {
            self.shapes.iter().any(|shape| {
                matches!(shape, egui::Shape::Rect(button) if button.fill == ARC && button.rect.contains(rect.center()))
            })
        }

        fn texts(&self) -> Vec<String> {
            self.shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect()
        }

        /// Each painted text with where it was drawn and its colour.
        fn text_shapes(&self) -> Vec<(String, Rect, Color32)> {
            self.shapes
                .iter()
                .filter_map(|shape| match shape {
                    egui::Shape::Text(text) => {
                        let color = text.galley.job.sections.first().map_or(Color32::PLACEHOLDER, |s| s.format.color);
                        Some((text.galley.text().to_owned(), text.visual_bounding_rect(), color))
                    }
                    _ => None,
                })
                .collect()
        }

        /// Where `text` was painted, if it was.
        fn find_text(&self, text: &str) -> Option<Rect> {
            self.text_shapes().into_iter().find(|(painted, _, _)| painted == text).map(|(_, rect, _)| rect)
        }

        fn type_text(&mut self, text: &str) {
            self.input.events.push(egui::Event::Text(text.to_owned()));
        }

        fn press_key(&mut self, key: egui::Key) {
            for pressed in [true, false] {
                let modifiers = self.input.modifiers;
                self.input.events.push(egui::Event::Key { key, physical_key: None, pressed, repeat: false, modifiers });
            }
        }

        fn take_edits(&self) -> Vec<Edit> {
            std::mem::take(&mut *self.host.edits.lock().unwrap())
        }
    }

    struct Editor {
        plugin: crate::Haunter,
        screen: Screen,
        state: EditorState,
        presets: TempDir,
    }

    impl Editor {
        fn new(with_sample: bool) -> Self {
            Self::scaled(with_sample, 1.0)
        }

        fn scaled(with_sample: bool, pixels_per_point: f32) -> Self {
            let presets = TempDir::new();
            let plugin = crate::Haunter::with_preset_dir(presets.path().to_owned());
            if with_sample {
                let kick = (0..4_800).map(|n| (n as f32 * 0.01).sin() * (1.0 - n as f32 / 4_800.0)).collect();
                let sample =
                    crate::sample::EmbeddedSample { name: "kick.wav".into(), sample_rate: 48_000.0, channels: vec![kick] };
                crate::install_sample(sample, &plugin.params, &plugin.shared);
                plugin.shared.playhead.store(0.3, Ordering::Relaxed);
                plugin.shared.gain_reduction_db.store(9.0, Ordering::Relaxed);
            }
            Editor { plugin, screen: Screen::new(pixels_per_point), state: EditorState::default(), presets }
        }

        fn frame(&mut self) -> Drawn {
            let Editor { plugin, screen, state, .. } = self;
            screen.frame(|context, setter| draw(context, setter, &plugin.params, &plugin.shared, state))
        }

        /// The first pass measures text, the second lays out with those sizes.
        fn settle(&mut self) -> Drawn {
            self.frame();
            self.frame()
        }

        /// Slides the cursor onto `to` over a few frames, the way a hand moves it.
        fn move_to(&mut self, to: Pos2) {
            let from = self.screen.cursor.unwrap_or(Pos2::new(WIDTH as f32 * 0.5, 4.0));
            for step in 1..=6 {
                self.screen.cursor_moved(from.lerp(to, step as f32 / 6.0));
                self.frame();
            }
        }

        fn wait(&mut self, seconds: f64) {
            for _ in 0..(seconds / 0.015).ceil() as usize {
                self.frame();
            }
        }

        /// Clicks the way people do: moves onto `pos`, rests there long enough for the tooltip,
        /// holds the button down for about a tenth of a second, and lets go.
        fn click_at(&mut self, pos: Pos2) {
            self.move_to(pos);
            self.wait(0.7);
            self.screen.left_button(true);
            self.wait(0.1);
            self.screen.left_button(false);
            self.frame();
        }

        /// Presses where the cursor is, drags by `by`, and lets go.
        fn drag(&mut self, by: egui::Vec2) {
            let from = self.screen.cursor.expect("the cursor is over the window");
            self.screen.left_button(true);
            self.frame();
            for step in 1..=4 {
                self.screen.cursor_moved(from + by * (step as f32 / 4.0));
                self.frame();
            }
            self.screen.left_button(false);
            self.frame();
        }
    }

    /// The knob centres of a row drawn by `knob_row`.
    fn knob_centres(row: Rect) -> impl Iterator<Item = Pos2> {
        (0..KNOB_COLUMNS as usize)
            .map(move |index| Pos2::new(row.left() + row.width() * (index as f32 + 0.5) / KNOB_COLUMNS, row.top() + 40.0))
    }

    /// Asserts `edits` is one open, held, and closed edit of `param` to `value`.
    fn assert_one_edit(edits: &[Edit], param: ParamPtr, value: f32) {
        assert!(edits.len() >= 3, "{edits:?}");
        assert_eq!(edits[0], Edit::Begin(param), "{edits:?}");
        assert_eq!(edits[edits.len() - 1], Edit::End(param), "{edits:?}");
        assert!(edits[1..edits.len() - 1].iter().all(|edit| *edit == Edit::Set(param, value)), "{edits:?}");
    }

    #[test]
    fn every_control_fits_in_the_window() {
        for (with_sample, naming) in [(false, false), (true, false), (true, true)] {
            let mut editor = Editor::new(with_sample);
            if naming {
                editor.state.naming = Some("A rather long preset name".into());
            }
            let drawn = editor.settle();
            let used = drawn.used;
            assert!(used.max.x <= WIDTH as f32, "too wide: {used:?}");
            assert!(used.max.y <= HEIGHT as f32, "too tall: {used:?}");
            assert!(used.max.y > HEIGHT as f32 * 0.8, "unexpectedly short layout: {used:?}");
            if !naming {
                let load = editor.screen.find_text("Load").expect("Load is drawn");
                let title = editor.screen.find_text("Haunter").expect("the title is drawn");
                assert!(title.right() < drawn.preset_menu.left(), "the title, then the preset menu");
                assert!(drawn.preset_menu.right() <= drawn.save.left(), "the menu is left of Save");
                assert!(drawn.save.right() <= load.left(), "Save is left of Load");
            }
        }
    }

    #[test]
    fn a_click_flips_restart_and_reverse_before_the_host_answers() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        editor.screen.take_edits();
        let restart = editor.plugin.params.restart.as_ptr();
        let reverse = editor.plugin.params.reverse.as_ptr();
        assert!(editor.screen.lit(drawn.restart) && !editor.screen.lit(drawn.reverse));

        editor.move_to(drawn.reverse.center());
        editor.wait(0.7);
        assert!(
            editor.screen.texts().iter().any(|text| text.starts_with("Plays the region backwards")),
            "the tooltip is up, as it is when someone rests on the button before clicking"
        );
        editor.screen.left_button(true);
        editor.frame();
        assert_eq!(editor.screen.take_edits(), [Edit::Begin(reverse), Edit::Set(reverse, 1.0)], "the press opens the edit");
        for _ in 0..5 {
            editor.frame();
        }
        assert_eq!(editor.screen.take_edits(), [Edit::Set(reverse, 1.0); 5], "held open like a knob drag");
        assert!(editor.screen.lit(drawn.reverse), "lit while the button is still down");
        editor.screen.left_button(false);
        editor.frame();
        assert_eq!(editor.screen.take_edits(), [Edit::Set(reverse, 1.0), Edit::End(reverse)], "the release closes it");
        editor.wait(1.0);
        assert!(editor.screen.take_edits().is_empty());
        assert!(!editor.plugin.params.reverse.value(), "the host has not sent the edit back yet");
        assert!(editor.screen.lit(drawn.reverse), "but Reverse shows on");

        editor.click_at(drawn.restart.center());
        assert_one_edit(&editor.screen.take_edits(), restart, 0.0);
        assert!(!editor.screen.lit(drawn.restart), "Restart shows off");

        editor.click_at(drawn.reverse.center());
        assert_one_edit(&editor.screen.take_edits(), reverse, 0.0);
        assert!(!editor.screen.lit(drawn.reverse), "a second click turns Reverse back off");
    }

    #[test]
    fn a_press_and_release_inside_one_frame_still_flips() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        let reverse = editor.plugin.params.reverse.as_ptr();
        editor.move_to(drawn.reverse.center());
        editor.screen.take_edits();
        editor.screen.left_button(true);
        editor.screen.left_button(false);
        editor.frame();
        assert_eq!(editor.screen.take_edits(), [Edit::Begin(reverse), Edit::Set(reverse, 1.0), Edit::End(reverse)]);
        editor.frame();
        assert!(editor.screen.lit(drawn.reverse));
    }

    #[test]
    fn a_toggle_shows_the_parameter_again_once_the_host_has_had_blocks_to_apply_it() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        editor.click_at(drawn.reverse.center());
        editor.wait(2.0);
        assert!(editor.screen.lit(drawn.reverse), "no blocks yet, so the host has not had its chance");

        let blocks = &editor.plugin.shared.processed_blocks;
        blocks.fetch_add(HOST_GRACE_BLOCKS - 1, Ordering::Relaxed);
        editor.frame();
        assert!(editor.screen.lit(drawn.reverse));
        editor.plugin.shared.processed_blocks.fetch_add(1, Ordering::Relaxed);
        editor.frame();
        assert!(!editor.screen.lit(drawn.reverse), "the host dropped the edit, so the button shows the real value");
    }

    #[test]
    fn the_press_decides_what_a_click_does() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        let reverse = editor.plugin.params.reverse.as_ptr();
        editor.screen.take_edits();

        editor.move_to(drawn.reverse.center());
        editor.drag(egui::vec2(60.0, 0.0));
        assert_one_edit(&editor.screen.take_edits(), reverse, 1.0);

        let beside = Pos2::new(drawn.reverse.right() + 20.0, drawn.reverse.center().y);
        editor.click_at(beside);
        editor.drag(drawn.reverse.center() - beside);
        assert!(editor.screen.take_edits().is_empty(), "pressing beside the toggles does nothing");
    }

    #[test]
    fn the_toggles_work_on_a_scaled_display() {
        let mut editor = Editor::scaled(true, 1.5);
        let drawn = editor.settle();
        editor.screen.take_edits();
        editor.click_at(drawn.reverse.center());
        assert_one_edit(&editor.screen.take_edits(), editor.plugin.params.reverse.as_ptr(), 1.0);
        assert!(editor.screen.lit(drawn.reverse));
    }

    #[test]
    fn every_knob_turns_when_dragged() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        let p = &editor.plugin.params;
        let rows = [
            (drawn.upper_knobs, [p.rate.as_ptr(), p.depth.as_ptr(), p.attack.as_ptr(), p.release.as_ptr(), p.mix.as_ptr()]),
            (
                drawn.lower_knobs,
                [p.high_pass.as_ptr(), p.low_pass.as_ptr(), p.sample_volume.as_ptr(), p.blend.as_ptr(), p.dream_eater.as_ptr()],
            ),
        ];
        editor.screen.take_edits();
        for (row, knobs) in rows {
            for (centre, param) in knob_centres(row).zip(knobs) {
                editor.move_to(centre);
                editor.drag(egui::vec2(0.0, -40.0));
                let edits = editor.screen.take_edits();
                assert_eq!(edits.first(), Some(&Edit::Begin(param)), "{edits:?}");
                assert_eq!(edits.last(), Some(&Edit::End(param)), "{edits:?}");
                assert!(edits.iter().any(|edit| matches!(edit, Edit::Set(p, _) if *p == param)), "{edits:?}");
            }
        }
    }

    /// The Rate readout's texts and colours, from the painted shapes under the Rate knob.
    fn rate_readout(editor: &Editor, row: Rect) -> Vec<(String, Color32)> {
        let rate = knob_centres(row).next().unwrap();
        editor
            .screen
            .text_shapes()
            .into_iter()
            .filter(|(text, rect, _)| (rect.center().x - rate.x).abs() < 40.0 && row.contains(rect.center()) && text != "RATE")
            .map(|(text, _, color)| (text, color))
            .collect()
    }

    #[test]
    fn restart_off_locks_rate_and_shows_infinity() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        let rate = editor.plugin.params.rate.as_ptr();
        let rate_centre = knob_centres(drawn.upper_knobs).next().unwrap();
        assert_eq!(rate_readout(&editor, drawn.upper_knobs), [("1/4".to_owned(), INK)]);

        editor.click_at(drawn.restart.center());
        editor.screen.take_edits();
        let font_has_infinity = editor.screen.context.fonts(|fonts| fonts.has_glyphs(&egui::FontId::proportional(18.0), INFINITY));
        assert!(font_has_infinity, "egui's default font draws U+221E");
        assert_eq!(rate_readout(&editor, drawn.upper_knobs), [(INFINITY.to_owned(), MUTED)], "a grey infinity instead of 1/4");
        assert_eq!(editor.screen.discs(DISC).len(), 9, "Rate keeps its normal face, like every knob but Dream Eater");
        let rate_fills: Vec<Color32> = editor
            .screen
            .shapes
            .iter()
            .filter_map(|shape| match shape {
                egui::Shape::Circle(circle) if circle.center.distance(rate_centre) < 20.0 && circle.fill != Color32::TRANSPARENT => {
                    Some(circle.fill)
                }
                _ => None,
            })
            .collect();
        assert_eq!(rate_fills, [DISC], "nothing greys it out");

        editor.move_to(rate_centre);
        editor.drag(egui::vec2(0.0, -40.0));
        for pressed in [true, false, true, false] {
            editor.screen.left_button(pressed);
            editor.frame();
        }
        editor.screen.input.events.push(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 3.0),
            modifiers: egui::Modifiers::NONE,
        });
        editor.frame();
        let edits = editor.screen.take_edits();
        assert!(!edits.iter().any(|edit| matches!(edit, Edit::Begin(p) | Edit::Set(p, _) | Edit::End(p) if *p == rate)), "{edits:?}");

        editor.click_at(drawn.restart.center());
        assert_eq!(rate_readout(&editor, drawn.upper_knobs), [("1/4".to_owned(), INK)], "Restart on shows the fraction again");
        editor.screen.take_edits();
        editor.move_to(rate_centre);
        editor.drag(egui::vec2(0.0, -40.0));
        let edits = editor.screen.take_edits();
        assert!(edits.iter().any(|edit| matches!(edit, Edit::Set(p, _) if *p == rate)), "and Rate turns again: {edits:?}");
    }

    #[test]
    fn a_locked_knob_ignores_drags_and_double_clicks() {
        let plugin = crate::Haunter::default();
        let rate = plugin.params.rate.as_ptr();
        for turnable in [true, false] {
            let mut screen = Screen::new(1.0);
            let row = |screen: &mut Screen| {
                screen.frame(|context, setter| {
                    egui::CentralPanel::default()
                        .show(context, |ui| knob_row(ui, setter, &[("Rate", &plugin.params.rate, turnable, Face::Plain)]))
                        .inner
                })
            };
            row(&mut screen);
            let rect = row(&mut screen);
            let knob = knob_centres(rect).next().unwrap();
            screen.cursor_moved(knob);
            row(&mut screen);
            screen.left_button(true);
            row(&mut screen);
            for up in [20.0, 40.0] {
                screen.cursor_moved(knob - egui::vec2(0.0, up));
                row(&mut screen);
            }
            screen.left_button(false);
            row(&mut screen);
            screen.cursor_moved(knob);
            row(&mut screen);
            for _ in 0..2 {
                for pressed in [true, false] {
                    screen.left_button(pressed);
                    row(&mut screen);
                }
            }
            let edits = screen.take_edits();
            if turnable {
                assert!(edits.iter().any(|edit| matches!(edit, Edit::Set(p, _) if *p == rate)), "a turnable knob turns");
            } else {
                assert!(edits.is_empty(), "a locked knob does nothing: {edits:?}");
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
        editor.move_to(Pos2::new(last_column, row.top() + 40.0));
        editor.drag(egui::vec2(0.0, -40.0));
        let dream = editor.plugin.params.dream_eater.as_ptr();
        let edits = editor.screen.take_edits();
        assert!(edits.iter().any(|edit| matches!(edit, Edit::Set(p, value) if *p == dream && *value > 0.0)), "{edits:?}");
    }

    /// The text in the preset menu's button.
    fn shown_preset(editor: &mut Editor) -> String {
        let menu = editor.frame().preset_menu;
        let texts: Vec<String> = editor
            .screen
            .text_shapes()
            .into_iter()
            .filter(|(_, rect, _)| menu.contains(rect.center()))
            .map(|(text, _, _)| text)
            .collect();
        assert_eq!(texts.len(), 1, "{texts:?}");
        texts[0].clone()
    }

    /// Opens the preset menu and returns its entries from top to bottom, with where each is.
    fn open_preset_menu(editor: &mut Editor) -> Vec<(String, Rect)> {
        let menu = editor.frame().preset_menu;
        editor.click_at(menu.center());
        // egui lays the popup out unseen on its first frame.
        editor.frame();
        assert!(editor.state.menu_open, "the menu opens");
        let mut entries: Vec<(String, Rect)> = editor
            .screen
            .text_shapes()
            .into_iter()
            .filter(|(_, rect, _)| rect.top() > menu.bottom() && rect.left() >= menu.left() && rect.right() <= menu.right())
            .map(|(text, rect, _)| (text, rect))
            .collect();
        entries.sort_by(|a, b| a.1.top().total_cmp(&b.1.top()));
        // The popup's rows are close together; anything after a gap is the rest of the editor.
        let rows = entries.windows(2).take_while(|pair| pair[1].1.top() - pair[0].1.top() < 40.0).count() + 1;
        entries.truncate(rows);
        entries
    }

    fn close_preset_menu(editor: &mut Editor) {
        let menu = editor.frame().preset_menu;
        editor.click_at(menu.center());
        assert!(!editor.state.menu_open, "a second click closes it");
    }

    fn pick_preset(editor: &mut Editor, name: &str) {
        let entries = open_preset_menu(editor);
        let (_, rect) = entries
            .iter()
            .find(|(text, _)| text == name)
            .unwrap_or_else(|| panic!("{name} is not in the menu: {entries:?}"));
        editor.click_at(rect.center());
        editor.frame();
        assert!(!editor.state.menu_open, "choosing closes the menu");
    }

    fn another_sample() -> crate::sample::EmbeddedSample {
        let audio = (0..2_400).map(|n| (n as f32 * 0.02).sin() * 0.5).collect();
        crate::sample::EmbeddedSample { name: "snare.wav".into(), sample_rate: 48_000.0, channels: vec![audio] }
    }

    #[test]
    fn choosing_water_loads_its_embedded_sample_with_default_settings() {
        let mut editor = Editor::new(false);
        editor.settle();
        assert_eq!(shown_preset(&mut editor), "--", "a new instance has no preset and no sample");
        let entries: Vec<String> = open_preset_menu(&mut editor).into_iter().map(|(text, _)| text).collect();
        assert_eq!(entries, ["--", "Water"]);
        close_preset_menu(&mut editor);
        editor.screen.take_edits();

        pick_preset(&mut editor, "Water");
        let params = editor.plugin.params.clone();
        let sample = params.sample.read(|s| s.cloned()).expect("Water loads a sample");
        assert_eq!(sample.name, "Water");
        assert_eq!(sample, crate::preset::Preset::factory("Water").unwrap().sample.unwrap(), "the sample built into the plugin");
        assert_eq!(params.preset.get().choice, Choice::Factory("Water".into()));
        assert_eq!(shown_preset(&mut editor), "Water");
        assert!(editor.screen.texts().iter().any(|text| text.starts_with("Water · 4179 ms")), "{:?}", editor.screen.texts());

        let edits = editor.screen.take_edits();
        for (id, ptr, _) in params.param_map() {
            let default = unsafe { ptr.default_normalized_value() };
            let sent: Vec<Edit> = edits
                .iter()
                .filter(|edit| matches!(edit, Edit::Begin(p) | Edit::Set(p, _) | Edit::End(p) if *p == ptr))
                .copied()
                .collect();
            assert_eq!(sent, [Edit::Begin(ptr), Edit::Set(ptr, default), Edit::End(ptr)], "{id} goes to its default");
        }

        editor.plugin.adopt_pending_sample();
        let playing = editor.plugin.engine.sample().expect("the audio thread plays it");
        assert_eq!(playing.frames(), (184_275.0 * 48_000.0 / 44_100.0f64).round() as usize, "resampled to the host rate");

        pick_preset(&mut editor, "--");
        assert!(params.sample.read(|s| s.is_none()), "-- means no sample");
        assert_eq!(shown_preset(&mut editor), "--");
        editor.plugin.adopt_pending_sample();
        assert!(editor.plugin.engine.sample().is_none());
    }

    #[test]
    fn choosing_a_user_preset_sends_its_settings_and_loads_its_sample() {
        let mut editor = Editor::new(false);
        editor.settle();
        let mut preset = crate::preset::Preset { values: Default::default(), sample: Some(another_sample()) };
        for (id, value) in [("rate", 4.0), ("restart", 0.0), ("reverse", 1.0), ("start", 0.125), ("depth", 0.4), ("release", 321.5)] {
            preset.values.insert(id.to_owned(), value);
        }
        editor.plugin.shared.user_presets.save("Snare Gate", &preset).unwrap();
        let entries: Vec<String> = open_preset_menu(&mut editor).into_iter().map(|(text, _)| text).collect();
        assert_eq!(entries, ["--", "Water", "Snare Gate"], "user presets come after the factory ones");
        close_preset_menu(&mut editor);
        editor.screen.take_edits();

        pick_preset(&mut editor, "Snare Gate");
        let params = editor.plugin.params.clone();
        assert_eq!(params.sample.read(|s| s.cloned()), preset.sample);
        let edits = editor.screen.take_edits();
        for (id, ptr, _) in params.param_map() {
            let expected = unsafe { ptr.preview_normalized(preset.value(&id, &ptr)) };
            assert!(edits.contains(&Edit::Set(ptr, expected)), "{id} is sent as {expected}: {edits:?}");
        }
        assert_eq!(shown_preset(&mut editor), "Snare Gate", "not changed while the host applies the settings");
        editor.plugin.shared.processed_blocks.fetch_add(HOST_GRACE_BLOCKS, Ordering::Relaxed);
        assert_eq!(shown_preset(&mut editor), "Snare Gate*", "the test host never applies them, so they differ");
    }

    #[test]
    fn the_preset_name_gets_a_star_after_a_change_until_saved_or_chosen_again() {
        let mut editor = Editor::new(false);
        editor.settle();
        pick_preset(&mut editor, "Water");
        assert_eq!(shown_preset(&mut editor), "Water");

        crate::install_sample(another_sample(), &editor.plugin.params, &editor.plugin.shared);
        assert_eq!(shown_preset(&mut editor), "Water*", "a replaced sample");
        pick_preset(&mut editor, "Water");
        assert_eq!(shown_preset(&mut editor), "Water", "choosing it again puts it back");

        // The test host never applies edits, so move what the preset set instead of the knob.
        editor.plugin.shared.processed_blocks.fetch_add(HOST_GRACE_BLOCKS, Ordering::Relaxed);
        for (id, value) in [("depth", 0.3), ("restart", 0.0), ("reverse", 1.0), ("start", 0.2)] {
            let params = &editor.plugin.params;
            let original = params.preset.get();
            let mut moved = original.clone();
            moved.values.insert(id.to_owned(), value);
            params.preset.set(moved);
            assert_eq!(shown_preset(&mut editor), "Water*", "{id} changed");
            editor.plugin.params.preset.set(original);
            assert_eq!(shown_preset(&mut editor), "Water");
        }

        crate::install_sample(another_sample(), &editor.plugin.params, &editor.plugin.shared);
        assert_eq!(shown_preset(&mut editor), "Water*");
        let save = editor.frame().save;
        editor.click_at(save.center());
        assert!(editor.state.naming.is_some(), "Save asks for a name");
        editor.frame();
        assert!(editor.screen.find_text("Save as").is_some() && editor.screen.find_text("OK").is_some());
        editor.screen.type_text("Water");
        editor.frame();
        editor.screen.press_key(egui::Key::Enter);
        editor.frame();
        let error = editor.state.save_error.clone().expect("a factory name is refused");
        editor.frame();
        assert!(editor.screen.find_text(&error).is_some(), "the reason is shown");
        for _ in 0.."Water".len() {
            editor.screen.press_key(egui::Key::Backspace);
        }
        editor.screen.type_text("Snare Ghost");
        editor.frame();
        editor.screen.press_key(egui::Key::Enter);
        editor.frame();
        assert!(editor.state.naming.is_none(), "saved: {:?}", editor.state.save_error);
        assert!(editor.presets.path().join("Snare Ghost.json").is_file());
        assert_eq!(shown_preset(&mut editor), "Snare Ghost", "saving clears the star");

        crate::install_sample(kick_like(), &editor.plugin.params, &editor.plugin.shared);
        assert_eq!(shown_preset(&mut editor), "Snare Ghost*");
        pick_preset(&mut editor, "Snare Ghost");
        assert_eq!(shown_preset(&mut editor), "Snare Ghost");
        assert_eq!(editor.plugin.params.sample.read(|s| s.cloned()), Some(another_sample()), "with its own sample back");

        // Saving again under the same name, from the prompt prefilled with it, replaces it.
        let save = editor.frame().save;
        editor.click_at(save.center());
        assert_eq!(editor.state.naming.as_deref(), Some("Snare Ghost"));
        editor.frame();
        let ok = editor.screen.find_text("OK").unwrap();
        editor.click_at(ok.center());
        assert!(editor.state.naming.is_none());
        assert_eq!(editor.plugin.shared.user_presets.list(), ["Snare Ghost"]);

        let save = editor.frame().save;
        editor.click_at(save.center());
        editor.screen.press_key(egui::Key::Escape);
        editor.frame();
        assert!(editor.state.naming.is_none(), "Escape cancels");
        let cancel_and_back = editor.frame();
        assert_ne!(cancel_and_back.preset_menu, Rect::NOTHING, "the preset controls come back");
    }

    fn kick_like() -> crate::sample::EmbeddedSample {
        let audio = (0..4_800).map(|n| (n as f32 * 0.005).sin() * (1.0 - n as f32 / 4_800.0)).collect();
        crate::sample::EmbeddedSample { name: "boom.wav".into(), sample_rate: 48_000.0, channels: vec![audio] }
    }
}
