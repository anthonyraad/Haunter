use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor,
    egui::{self, Color32, CornerRadius, Pos2, Rect, Stroke, StrokeKind},
    EguiState,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::dream::KeyChoice;
use crate::dsp::Direction;
use crate::preset::{self, Choice};
use crate::{apply_preset, load_file, read_preset, save_preset, HaunterParams, Shared};

pub const WIDTH: u32 = 760;
pub const HEIGHT: u32 = 600;

const INK: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
const SUBTLE: Color32 = Color32::from_rgb(0x3a, 0x3a, 0x3a);
const OUTLINE: Color32 = Color32::from_rgb(0x4a, 0x4a, 0x4a);
const DISC: Color32 = Color32::from_rgb(0xf6, 0xf5, 0xfc);
const TRACK: Color32 = Color32::from_rgb(0xba, 0xb5, 0xd1);
/// The accent, #7C6EBB.
const ARC: Color32 = Color32::from_rgb(0x7c, 0x6e, 0xbb);
/// The accent lightened, behind selected menu items and text.
const ARC_LIGHT: Color32 = Color32::from_rgb(0xcb, 0xc5, 0xe4);
/// The accent darkened, for selected text, focus outlines, and the text cursor.
const ARC_DARK: Color32 = Color32::from_rgb(0x4a, 0x42, 0x70);
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
const AUDIO_EXTENSIONS: [&str; 6] = ["wav", "wave", "aif", "aiff", "aifc", "mp3"];
/// How far either side of a Start or End handle a press grabs it instead of loading a file.
const HANDLE_REACH: f32 = 12.0;

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
            visuals.selection.bg_fill = ARC_LIGHT;
            visuals.selection.stroke = Stroke::new(1.0_f32, ARC_DARK);
            visuals.text_cursor.stroke = Stroke::new(2.0_f32, ARC_DARK);
            visuals.hyperlink_color = ARC;
            context.set_visuals(visuals);
            configure(context);
        },
        move |context, setter, state| {
            if draw(context, setter, &params, &shared, state).browse {
                open_file_dialog(params.clone(), shared.clone(), state.dialog_open.clone());
            }
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
    waveform: Rect,
    /// The waveform was clicked to choose a file.
    browse: bool,
    restart: Rect,
    direction: Rect,
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
    let busy = shared.loading.load(Ordering::Acquire) || state.dialog_open.load(Ordering::Acquire);

    egui::CentralPanel::default()
        .show(context, |ui| {
            let [preset_menu, save] = header(ui, setter, params, shared, state, busy);
            ui.add_space(8.0);
            let can_browse = !busy && !hovering_files;
            let (waveform, browse) = waveform(ui, setter, params, shared, state, hovering_files, can_browse);
            ui.add_space(10.0);
            let ([restart, direction], restart_on) = toggles_and_meter(ui, setter, params, shared, state);
            ui.add_space(8.0);
            // Without Restart the region just loops, so the grid spacing does nothing.
            let (upper_knobs, _) = knob_row(ui, setter, &[
                ("Rate", &params.rate, restart_on, Face::Plain),
                ("Depth", &params.depth, true, Face::Plain),
                ("Attack", &params.attack, true, Face::Plain),
                ("Release", &params.release, true, Face::Plain),
                ("Mix", &params.mix, true, Face::Plain),
            ]);
            let dream_level = shared.dream_level.load(Ordering::Relaxed);
            let (lower_knobs, lower) = knob_row(ui, setter, &[
                ("HP", &params.high_pass, true, Face::Plain),
                ("LP", &params.low_pass, true, Face::Plain),
                ("Sample Vol", &params.sample_volume, true, Face::Plain),
                ("Blend", &params.blend, true, Face::Plain),
                ("Dream Eater", &params.dream_eater, true, Face::Eater { progress: dream_level }),
            ]);
            key_menu(&lower[4], params);
            Drawn { used: ui.min_rect(), preset_menu, save, waveform, browse, restart, direction, upper_knobs, lower_knobs }
        })
        .inner
}

fn is_audio_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTENSIONS.iter().any(|known| ext.eq_ignore_ascii_case(known)))
}

/// The title, then the preset menu and Save, or the Save prompt in their place. Under them, the
/// loaded sample or what went wrong, when there is either. Returns where the preset menu and Save
/// were drawn.
fn header(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &Arc<HaunterParams>,
    shared: &Arc<Shared>,
    state: &mut EditorState,
    busy: bool,
) -> [Rect; 2] {
    let mut drawn = [Rect::NOTHING; 2];
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Haunter").size(22.0).strong().color(INK));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if state.naming.is_some() {
                naming_prompt(ui, params, shared, state);
                return;
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
    let line = if let Some(error) = &state.save_error {
        Some((error.clone(), ERROR))
    } else {
        let status = shared.status.lock().unwrap().clone();
        if !status.is_empty() {
            Some((status, SUBTLE))
        } else {
            let overview = shared.overview.lock().unwrap();
            overview.as_ref().map(|overview| (format!("{} · {:.0} ms", overview.name, overview.seconds * 1000.0), SUBTLE))
        }
    };
    if let Some((line, color)) = line {
        ui.label(egui::RichText::new(line).color(color));
    }
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

/// Paints a closed menu's text in the middle of its button. egui's ComboBox puts the text at the
/// left, so the menu is given the same text in a transparent colour, which keeps the button the
/// size it was, and this paints it again centred, both ways. Text too wide to centre without
/// reaching the arrow ends just left of it.
fn paint_menu_text(ui: &egui::Ui, button: Rect, text: egui::RichText, truncate: bool) {
    let spacing = ui.spacing();
    let content = button.shrink2(spacing.button_padding);
    let room = (content.width() - spacing.icon_spacing - spacing.icon_width).max(0.0);
    let mode = if truncate { egui::TextWrapMode::Truncate } else { egui::TextWrapMode::Extend };
    let galley = egui::WidgetText::from(text).into_galley(ui, Some(mode), room, egui::TextStyle::Button);
    let size = galley.size();
    let left = (button.center().x - size.x / 2.0).min(content.left() + room - size.x).max(content.left());
    let top = content.center().y - size.y / 2.0;
    ui.painter().galley(Pos2::new(left, top), galley, INK);
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
            let menu = egui::ComboBox::from_id_salt("preset")
                .width(190.0)
                // Room for the factory presets and a dozen saved ones before it scrolls.
                .height(480.0)
                .truncate()
                .selected_text(egui::RichText::new(&label).size(14.0).color(Color32::TRANSPARENT))
                .show_ui(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
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
                });
            paint_menu_text(ui, menu.response.rect, egui::RichText::new(label).size(14.0).color(INK), true);
            menu
        })
        .inner;
    let was_open = state.menu_open;
    state.menu_open = menu.inner.is_some();
    if was_open && !state.menu_open {
        // egui caps a reopened popup at the size it had last time, which would scroll presets
        // saved since then out of view.
        ui.memory_mut(|memory| memory.reset_areas());
    }
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
            .add_filter("WAV, AIFF, or MP3", &AUDIO_EXTENSIONS)
            .pick_file();
        dialog_open.store(false, Ordering::Release);
        if let Some(path) = picked {
            load_file(path, params, shared);
        }
    });
}

/// The sample with its Start and End handles. A click that isn't on a handle, or anywhere while
/// there is no sample, asks for a file when `can_browse`. Returns where it was drawn and whether
/// it was clicked to choose a file.
fn waveform(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    params: &HaunterParams,
    shared: &Shared,
    state: &mut EditorState,
    hovering_files: bool,
    can_browse: bool,
) -> (Rect, bool) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 180.0), egui::Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect(rect, CornerRadius::same(6), DISC, Stroke::new(1.0_f32, OUTLINE), StrokeKind::Inside);
    // The click that closes the preset menu shouldn't also open the file browser.
    let can_browse = can_browse && !state.menu_open;

    let overview = shared.overview.lock().unwrap().clone();
    let inner = rect.shrink2(egui::vec2(10.0, 14.0));
    let x_of = |fraction: f32| inner.left() + fraction.clamp(0.0, 1.0) * inner.width();
    let fraction_of = |x: f32| ((x - inner.left()) / inner.width()).clamp(0.0, 1.0);

    let Some(overview) = overview else {
        painter.text(rect.center(), egui::Align2::CENTER_CENTER, "Drop a sample here", egui::FontId::proportional(15.0), SUBTLE);
        if can_browse && response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        drop_overlay(&painter, rect, hovering_files);
        return (rect, can_browse && response.clicked());
    };

    let start = params.start.modulated_plain_value();
    let end = params.end.modulated_plain_value();
    let (low, high) = (start.min(end), start.max(end));
    let on_handle = |x: f32| (x - x_of(start)).abs() < HANDLE_REACH || (x - x_of(end)).abs() < HANDLE_REACH;
    let browse = can_browse && response.clicked() && response.interact_pointer_pos().is_some_and(|pointer| !on_handle(pointer.x));

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
        if on_handle(pointer.x) || state.dragging.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
        } else if can_browse {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
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
    let shade = Color32::from_rgba_unmultiplied(0x62, 0x5e, 0x72, 40);
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
    (rect, browse)
}

fn handle_param(params: &HaunterParams, handle: Handle) -> &FloatParam {
    match handle {
        Handle::Start => &params.start,
        Handle::End => &params.end,
    }
}

fn drop_overlay(painter: &egui::Painter, rect: Rect, hovering_files: bool) {
    if hovering_files {
        painter.rect_filled(rect, CornerRadius::same(6), Color32::from_rgba_unmultiplied(0x7c, 0x6e, 0xbb, 60));
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

/// Editor frames (baseview ticks every 15 ms) a Direction edit stays open at least, about as long
/// as a click on a toggle holds its edit open.
const MIN_HOLD_FRAMES: u32 = 8;
/// Editor frames after which a Direction edit is closed even if the host never applied it, about
/// a second.
const MAX_HOLD_FRAMES: u32 = 66;

/// A Direction edit the host has been sent but has not applied to the parameter yet.
#[derive(Clone, Copy)]
struct Choosing {
    target: Direction,
    /// The edit is still open and is sent again every frame.
    open: bool,
    /// Frames the edit has been open.
    frames: u32,
    /// `Shared::processed_blocks` when the edit was closed.
    closed_at: u64,
}

/// The Direction menu, looking like the preset menu. Returns where it was drawn.
///
/// Choosing from a menu is a single click, but while the host is processing, nih-plug's VST3
/// wrapper leaves the parameter alone and waits for the host to send the edit back, and an edit
/// opened and closed in the same frame never comes back from Cubase. So, like a toggle held down
/// or a knob drag, the edit opens on the choice and is sent again every frame until the
/// parameter has the new value (or about a second has passed), and only then closed. The menu
/// shows the choice until the host applies it, or has had a few blocks after the edit closed to.
fn direction_menu(ui: &mut egui::Ui, setter: &ParamSetter, param: &EnumParam<Direction>, shared: &Shared) -> Rect {
    let memory = ui.make_persistent_id("direction");
    let blocks = shared.processed_blocks.load(Ordering::Relaxed);
    let choosing = ui.memory(|mem| mem.data.get_temp::<Choosing>(memory)).filter(|edit| {
        edit.open || (param.value() != edit.target && blocks < edit.closed_at + HOST_GRACE_BLOCKS)
    });
    let shown = choosing.map_or(param.value(), |edit| edit.target);
    let name = |direction: Direction| Direction::variants()[direction.to_index()];

    let mut picked = None;
    let menu = ui
        .scope(|ui| {
            ui.spacing_mut().interact_size.y = 26.0;
            let widgets = &mut ui.visuals_mut().widgets;
            for look in [&mut widgets.inactive, &mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
                look.weak_bg_fill = DISC;
                look.bg_stroke = Stroke::new(1.0_f32, OUTLINE);
            }
            let menu = egui::ComboBox::from_id_salt("direction")
                .width(110.0)
                .selected_text(egui::RichText::new(name(shown)).size(13.0).color(Color32::TRANSPARENT))
                .show_ui(ui, |ui| {
                    ui.with_layout(egui::Layout::top_down_justified(egui::Align::Center), |ui| {
                        for direction in [Direction::Forward, Direction::Backward, Direction::Levitate] {
                            if ui.selectable_label(direction == shown, name(direction)).clicked() {
                                picked = Some(direction);
                            }
                        }
                    })
                });
            paint_menu_text(ui, menu.response.rect, egui::RichText::new(name(shown)).size(13.0).color(INK), false);
            menu
        })
        .inner;
    menu.response.clone().on_hover_text(
        "Forward plays the region from S to E.\nBackward plays it from E to S, turning round at once when chosen.\nLevitate bounces between S and E.",
    );

    let choosing = match (picked, choosing) {
        // A new choice while an edit is open carries on with the same edit.
        (Some(target), Some(edit)) if edit.open => {
            setter.set_parameter(param, target);
            Some(Choosing { target, frames: 1, ..edit })
        }
        (Some(target), _) => {
            setter.begin_set_parameter(param);
            setter.set_parameter(param, target);
            Some(Choosing { target, open: true, frames: 1, closed_at: blocks })
        }
        (None, Some(mut edit)) if edit.open => {
            setter.set_parameter(param, edit.target);
            edit.frames += 1;
            let applied = param.value() == edit.target;
            if (applied && edit.frames >= MIN_HOLD_FRAMES) || edit.frames >= MAX_HOLD_FRAMES {
                setter.end_set_parameter(param);
                edit.open = false;
                edit.closed_at = blocks;
            }
            Some(edit)
        }
        (None, other) => other,
    };
    if choosing.is_some_and(|edit| edit.open) {
        // Keeps frames coming while the edit is open, whatever egui would otherwise repaint.
        ui.ctx().request_repaint();
    }
    ui.memory_mut(|mem| match choosing {
        Some(edit) => mem.data.insert_temp(memory, edit),
        None => mem.data.remove::<Choosing>(memory),
    });
    menu.response.rect
}

/// Returns where the Restart toggle and the Direction menu were drawn, and whether Restart shows
/// on.
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
        let direction = direction_menu(ui, setter, &params.direction, shared);

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
        ([restart, direction], restart_on)
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
    /// (0..1, the sample's live sweep) filling the white arc in the accent colour from the start.
    Eater { progress: f32 },
}

trait KnobParam {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str, turnable: bool, face: Face) -> egui::Response;
}

impl<P: Param> KnobParam for P {
    fn draw(&self, ui: &mut egui::Ui, setter: &ParamSetter, name: &str, turnable: bool, face: Face) -> egui::Response {
        draw_knob(ui, setter, self, name, turnable, face)
    }
}

/// A knob that isn't `turnable` looks the same but takes no input, and reads a grey infinity.
/// Only the left button turns it.
fn draw_knob<P: Param>(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    param: &P,
    name: &str,
    turnable: bool,
    face: Face,
) -> egui::Response {
    let width = ui.available_width().max(88.0);
    let sense = if turnable { egui::Sense::click_and_drag() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 112.0), sense);
    let normalized = param.modulated_normalized_value();
    let memory = ui.id().with(name);

    if response.drag_started_by(egui::PointerButton::Primary) {
        setter.begin_set_parameter(param);
        ui.memory_mut(|mem| {
            mem.data.insert_temp(memory.with("start"), normalized);
            mem.data.insert_temp(memory.with("drag"), 0.0f32);
            mem.data.insert_temp(memory.with("turning"), true);
        });
    }
    let turning = ui.memory(|mem| mem.data.get_temp::<bool>(memory.with("turning")).unwrap_or(false));
    if turning && response.dragged() {
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
    if turning && response.drag_stopped() {
        setter.end_set_parameter(param);
        ui.memory_mut(|mem| mem.data.remove::<bool>(memory.with("turning")));
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
    response
}

/// Right-clicking Dream Eater chooses the key its resonances are tuned to. Nothing on the panel
/// shows the choice.
fn key_menu(knob: &egui::Response, params: &HaunterParams) {
    knob.context_menu(|ui| {
        let current = params.dream_key.get();
        for choice in KeyChoice::all() {
            if ui.selectable_label(choice == current, choice.label()).clicked() {
                params.dream_key.set(choice);
                ui.close_menu();
            }
        }
    });
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
/// area and each knob's response.
fn knob_row(
    ui: &mut egui::Ui,
    setter: &ParamSetter,
    knobs: &[(&str, &dyn KnobParam, bool, Face)],
) -> (Rect, Vec<egui::Response>) {
    let width = ui.available_width();
    let height = 118.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let mut responses = Vec::with_capacity(knobs.len());
    for (index, (name, param, turnable, face)) in knobs.iter().enumerate() {
        let center_x = rect.left() + width * (index as f32 + 0.5) / KNOB_COLUMNS;
        let slot = Rect::from_center_size(Pos2::new(center_x, rect.center().y), egui::vec2(120.0, height));
        let builder = egui::UiBuilder::new()
            .id_salt(*name)
            .max_rect(slot)
            .layout(egui::Layout::top_down(egui::Align::Center));
        responses.push(ui.allocate_new_ui(builder, |ui| param.draw(ui, setter, name, *turnable, *face)).inner);
    }
    (rect, responses)
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
        /// The mouse cursor the last frame asked for.
        cursor_icon: egui::CursorIcon,
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
            Screen {
                host: Host::default(),
                context,
                input,
                cursor: None,
                time: 0.0,
                shapes: Vec::new(),
                cursor_icon: egui::CursorIcon::Default,
            }
        }

        /// baseview's `CursorMoved`.
        fn cursor_moved(&mut self, pos: Pos2) {
            self.cursor = Some(pos);
            self.input.events.push(egui::Event::PointerMoved(pos));
        }

        /// baseview's `ButtonPressed` or `ButtonReleased` for the left button.
        fn left_button(&mut self, pressed: bool) {
            self.button(egui::PointerButton::Primary, pressed);
        }

        fn right_button(&mut self, pressed: bool) {
            self.button(egui::PointerButton::Secondary, pressed);
        }

        fn button(&mut self, button: egui::PointerButton, pressed: bool) {
            if let Some(pos) = self.cursor {
                let modifiers = self.input.modifiers;
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
            self.cursor_icon = output.platform_output.cursor_icon;
            fn flatten(shape: egui::Shape, into: &mut Vec<egui::Shape>) {
                match shape {
                    egui::Shape::Vec(shapes) => shapes.into_iter().for_each(|shape| flatten(shape, into)),
                    // Text painted fully transparent can't be seen, as the menus' own text isn't.
                    egui::Shape::Text(text)
                        if text.galley.job.sections.iter().all(|s| s.format.color == Color32::TRANSPARENT) => {}
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
        /// How many frames asked for the file browser.
        browsed: usize,
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
            Editor { plugin, screen: Screen::new(pixels_per_point), state: EditorState::default(), presets, browsed: 0 }
        }

        fn frame(&mut self) -> Drawn {
            let Editor { plugin, screen, state, .. } = self;
            let drawn = screen.frame(|context, setter| draw(context, setter, &plugin.params, &plugin.shared, state));
            self.browsed += drawn.browse as usize;
            drawn
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
            assert!(editor.screen.find_text("Load").is_none(), "there is no Load button");
            if !naming {
                let title = editor.screen.find_text("Haunter").expect("the title is drawn");
                assert!(title.right() < drawn.preset_menu.left(), "the title, then the preset menu");
                assert!(drawn.preset_menu.right() <= drawn.save.left(), "the menu is left of Save");
                assert!(drawn.save.right() > WIDTH as f32 - 20.0, "Save is at the right edge: {:?}", drawn.save);
                assert!(drawn.save.center().y < title.bottom() && drawn.save.bottom() > title.center().y, "on the title's line");
            }
        }
    }

    #[test]
    fn without_a_sample_nothing_sits_between_the_title_and_the_waveform() {
        let mut editor = Editor::new(false);
        let drawn = editor.settle();
        let title = editor.screen.find_text("Haunter").unwrap();
        let header_bottom = title.bottom().max(drawn.save.bottom()).max(drawn.preset_menu.bottom());
        assert!(drawn.waveform.top() - header_bottom < 14.0, "no gap: {header_bottom} then {:?}", drawn.waveform);
        let between: Vec<String> = editor
            .screen
            .text_shapes()
            .into_iter()
            .filter(|(_, rect, _)| rect.center().y > header_bottom && rect.center().y < drawn.waveform.top())
            .map(|(text, _, _)| text)
            .collect();
        assert!(between.is_empty(), "{between:?}");
        assert!(!editor.screen.texts().iter().any(|text| text.contains("one-shot")), "the old hint is gone");

        let painted: Vec<String> = editor
            .screen
            .text_shapes()
            .into_iter()
            .filter(|(_, rect, _)| drawn.waveform.contains(rect.center()))
            .map(|(text, _, _)| text)
            .collect();
        assert_eq!(painted, ["Drop a sample here"]);

        crate::install_sample(another_sample(), &editor.plugin.params, &editor.plugin.shared);
        let loaded = editor.settle();
        assert!(editor.screen.texts().iter().any(|text| text.starts_with("snare.wav · 50 ms")), "the sample is named under the title");
        assert!(loaded.waveform.top() > drawn.waveform.top());
        assert!(editor.screen.find_text("Drop a sample here").is_none());
    }

    #[test]
    fn a_click_anywhere_on_the_empty_waveform_opens_the_file_browser() {
        let mut editor = Editor::new(false);
        let waveform = editor.settle().waveform;
        for spot in [waveform.center(), waveform.left_top() + egui::vec2(4.0, 4.0), waveform.right_bottom() - egui::vec2(4.0, 4.0)] {
            editor.move_to(spot);
            assert_eq!(editor.screen.cursor_icon, egui::CursorIcon::PointingHand, "a hand over {spot:?}");
            let before = editor.browsed;
            editor.click_at(spot);
            assert_eq!(editor.browsed, before + 1, "one click at {spot:?} asks once");
        }
        let header = editor.frame().save;
        editor.click_at(Pos2::new(WIDTH as f32 * 0.4, header.center().y));
        let below = Pos2::new(waveform.center().x, waveform.bottom() + 6.0);
        editor.click_at(below);
        assert_eq!(editor.browsed, 3, "only the waveform opens it");
        assert_ne!(editor.screen.cursor_icon, egui::CursorIcon::PointingHand);
        assert!(editor.screen.take_edits().is_empty());

        editor.plugin.shared.loading.store(true, Ordering::Release);
        editor.click_at(waveform.center());
        editor.plugin.shared.loading.store(false, Ordering::Release);
        editor.state.dialog_open.store(true, Ordering::Release);
        editor.click_at(waveform.center());
        assert_eq!(editor.browsed, 3, "not while a file is loading or the browser is already open");
    }

    #[test]
    fn a_plain_click_on_a_loaded_waveform_opens_the_browser_but_handles_and_drags_do_not() {
        let mut editor = Editor::new(true);
        let waveform = editor.settle().waveform;
        let (start_x, end_x) = (waveform.left() + 10.0, waveform.right() - 10.0);
        let start = editor.plugin.params.start.as_ptr();

        editor.move_to(waveform.center());
        assert_eq!(editor.screen.cursor_icon, egui::CursorIcon::PointingHand);
        editor.screen.take_edits();
        editor.click_at(waveform.center());
        assert_eq!(editor.browsed, 1);
        assert!(editor.screen.take_edits().is_empty(), "a click moves no handle");

        for x in [start_x, start_x + 6.0, end_x - 6.0, end_x] {
            let spot = Pos2::new(x, waveform.center().y);
            editor.move_to(spot);
            assert_eq!(editor.screen.cursor_icon, egui::CursorIcon::ResizeHorizontal, "over a handle at {x}");
            editor.click_at(spot);
        }
        editor.click_at(Pos2::new(start_x, waveform.top() + 10.0));
        assert_eq!(editor.browsed, 1, "clicks on the handles and their tabs don't open it");

        editor.move_to(Pos2::new(start_x + 30.0, waveform.center().y));
        editor.drag(egui::vec2(80.0, 0.0));
        assert_eq!(editor.browsed, 1, "neither does dragging");
        let edits = editor.screen.take_edits();
        assert_eq!(edits.first(), Some(&Edit::Begin(start)), "the drag moved Start: {edits:?}");
        assert!(edits.iter().any(|edit| matches!(edit, Edit::Set(p, value) if *p == start && *value > 0.0)), "{edits:?}");
    }

    #[test]
    fn closing_the_preset_menu_on_the_waveform_does_not_open_the_browser() {
        let mut editor = Editor::new(false);
        let waveform = editor.settle().waveform;
        open_preset_menu(&mut editor);
        editor.click_at(Pos2::new(waveform.left() + 60.0, waveform.center().y));
        editor.frame();
        assert!(!editor.state.menu_open, "the click closed the menu");
        assert_eq!(editor.browsed, 0);
        editor.click_at(Pos2::new(waveform.left() + 60.0, waveform.center().y));
        assert_eq!(editor.browsed, 1, "the next click does");
    }

    #[test]
    fn a_click_flips_restart_before_the_host_answers() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        editor.screen.take_edits();
        let restart = editor.plugin.params.restart.as_ptr();
        assert!(editor.screen.lit(drawn.restart));

        editor.move_to(drawn.restart.center());
        editor.wait(0.7);
        assert!(
            editor.screen.texts().iter().any(|text| text.starts_with("On: the region starts again")),
            "the tooltip is up, as it is when someone rests on the button before clicking"
        );
        editor.screen.left_button(true);
        editor.frame();
        assert_eq!(editor.screen.take_edits(), [Edit::Begin(restart), Edit::Set(restart, 0.0)], "the press opens the edit");
        for _ in 0..5 {
            editor.frame();
        }
        assert_eq!(editor.screen.take_edits(), [Edit::Set(restart, 0.0); 5], "held open like a knob drag");
        assert!(!editor.screen.lit(drawn.restart), "off while the button is still down");
        editor.screen.left_button(false);
        editor.frame();
        assert_eq!(editor.screen.take_edits(), [Edit::Set(restart, 0.0), Edit::End(restart)], "the release closes it");
        editor.wait(1.0);
        assert!(editor.screen.take_edits().is_empty());
        assert!(editor.plugin.params.restart.value(), "the host has not sent the edit back yet");
        assert!(!editor.screen.lit(drawn.restart), "but Restart shows off");

        editor.click_at(drawn.restart.center());
        assert_one_edit(&editor.screen.take_edits(), restart, 1.0);
        assert!(editor.screen.lit(drawn.restart), "a second click turns Restart back on");
    }

    #[test]
    fn a_press_and_release_inside_one_frame_still_flips() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        let restart = editor.plugin.params.restart.as_ptr();
        editor.move_to(drawn.restart.center());
        editor.screen.take_edits();
        editor.screen.left_button(true);
        editor.screen.left_button(false);
        editor.frame();
        assert_eq!(editor.screen.take_edits(), [Edit::Begin(restart), Edit::Set(restart, 0.0), Edit::End(restart)]);
        editor.frame();
        assert!(!editor.screen.lit(drawn.restart));
    }

    #[test]
    fn a_toggle_shows_the_parameter_again_once_the_host_has_had_blocks_to_apply_it() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        editor.click_at(drawn.restart.center());
        editor.wait(2.0);
        assert!(!editor.screen.lit(drawn.restart), "no blocks yet, so the host has not had its chance");

        let blocks = &editor.plugin.shared.processed_blocks;
        blocks.fetch_add(HOST_GRACE_BLOCKS - 1, Ordering::Relaxed);
        editor.frame();
        assert!(!editor.screen.lit(drawn.restart));
        editor.plugin.shared.processed_blocks.fetch_add(1, Ordering::Relaxed);
        editor.frame();
        assert!(editor.screen.lit(drawn.restart), "the host dropped the edit, so the button shows the real value");
    }

    #[test]
    fn the_press_decides_what_a_click_does() {
        let mut editor = Editor::new(true);
        let drawn = editor.settle();
        let restart = editor.plugin.params.restart.as_ptr();
        editor.screen.take_edits();

        editor.move_to(drawn.restart.center());
        editor.drag(egui::vec2(60.0, 0.0));
        assert_one_edit(&editor.screen.take_edits(), restart, 0.0);

        let beside = Pos2::new(drawn.direction.right() + 20.0, drawn.restart.center().y);
        editor.click_at(beside);
        editor.drag(drawn.restart.center() - beside);
        assert!(editor.screen.take_edits().is_empty(), "pressing beside the toggles does nothing");
    }

    #[test]
    fn the_toggles_work_on_a_scaled_display() {
        let mut editor = Editor::scaled(true, 1.5);
        let drawn = editor.settle();
        editor.screen.take_edits();
        editor.click_at(drawn.restart.center());
        assert_one_edit(&editor.screen.take_edits(), editor.plugin.params.restart.as_ptr(), 0.0);
        assert!(!editor.screen.lit(drawn.restart));
        pick_direction(&mut editor, "Levitate");
        editor.wait(1.1);
        assert_one_edit(&editor.screen.take_edits(), editor.plugin.params.direction.as_ptr(), 1.0);
    }

    #[test]
    fn the_menus_centre_their_text() {
        for scale in [1.0, 1.5] {
            let mut editor = Editor::scaled(true, scale);
            let drawn = editor.settle();
            let centred = |what: &str, text: Rect, within: Rect| {
                assert!((text.center().x - within.center().x).abs() < 1.5, "{what} at {text:?} in {within:?}, scale {scale}");
                assert!((text.center().y - within.center().y).abs() < 3.0, "{what} at {text:?} in {within:?}, scale {scale}");
            };
            // A sample is loaded, so the preset shows its `*`.
            for (menu, name) in [(drawn.preset_menu, "--*"), (drawn.direction, "Forward")] {
                let texts: Vec<(String, Rect, Color32)> =
                    editor.screen.text_shapes().into_iter().filter(|(_, rect, _)| menu.contains(rect.center())).collect();
                assert_eq!(texts.len(), 1, "one text on the button: {texts:?}");
                assert_eq!(texts[0].0, name);
                centred(name, texts[0].1, menu);
            }

            let menu = editor.frame().direction;
            editor.click_at(menu.center());
            editor.frame();
            let items: Vec<(String, Rect)> = editor
                .screen
                .text_shapes()
                .into_iter()
                .filter(|(text, rect, _)| rect.top() > menu.bottom() - 1.0 && ["Forward", "Backward", "Levitate"].contains(&text.as_str()))
                .map(|(text, rect, _)| (text, rect))
                .collect();
            assert_eq!(items.len(), 3, "{items:?}");
            for (name, rect) in &items {
                assert!((rect.center().x - menu.center().x).abs() < 1.5, "{name} at {rect:?} under {menu:?}, scale {scale}");
            }
            editor.screen.press_key(egui::Key::Escape);
            editor.wait(0.2);

            let entries = open_preset_menu(&mut editor);
            assert!(entries.len() >= 6, "{entries:?}");
            for (name, rect) in &entries {
                assert!((rect.center().x - drawn.preset_menu.center().x).abs() < 1.5, "{name} at {rect:?}, scale {scale}");
            }
        }
    }

    /// The text the Direction menu shows.
    fn shown_direction(editor: &mut Editor) -> String {
        let menu = editor.frame().direction;
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

    fn pick_direction(editor: &mut Editor, name: &str) {
        let menu = editor.frame().direction;
        editor.click_at(menu.center());
        editor.frame();
        let entries: Vec<(String, Rect)> = editor
            .screen
            .text_shapes()
            .into_iter()
            .filter(|(_, rect, _)| rect.top() > menu.bottom() - 1.0)
            .map(|(text, rect, _)| (text, rect))
            .collect();
        let names: Vec<&str> = entries.iter().map(|(text, _)| text.as_str()).filter(|text| ["Forward", "Backward", "Levitate"].contains(text)).collect();
        assert_eq!(names, ["Forward", "Backward", "Levitate"], "the menu lists the directions in order");
        let (_, rect) = entries.iter().find(|(text, _)| text == name).unwrap();
        editor.click_at(rect.center());
        editor.frame();
    }

    #[test]
    fn the_direction_menu_sends_the_choice_and_shows_it_until_the_host_answers() {
        let mut editor = Editor::new(true);
        editor.settle();
        editor.screen.take_edits();
        let direction = editor.plugin.params.direction.as_ptr();
        assert_eq!(shown_direction(&mut editor), "Forward");

        let menu = editor.frame().direction;
        editor.move_to(menu.center());
        editor.wait(0.7);
        assert!(
            editor.screen.texts().iter().any(|text| text.starts_with("Forward plays")),
            "the menu explains the choices"
        );

        pick_direction(&mut editor, "Backward");
        let edits = editor.screen.take_edits();
        assert_eq!(edits[0], Edit::Begin(direction), "{edits:?}");
        assert!(edits[1..].iter().all(|edit| *edit == Edit::Set(direction, 0.5)), "held open and sent every frame: {edits:?}");
        assert_eq!(shown_direction(&mut editor), "Backward", "the menu shows the choice while the edit is open");
        editor.wait(1.1);
        let edits = editor.screen.take_edits();
        assert_eq!(edits.last(), Some(&Edit::End(direction)), "closed after about a second without an answer");
        editor.wait(1.0);
        assert!(editor.screen.take_edits().is_empty(), "and then left alone");
        assert_eq!(editor.plugin.params.direction.value(), Direction::Forward, "the host has not sent the edit back yet");
        assert_eq!(shown_direction(&mut editor), "Backward", "but the menu shows the choice");

        editor.plugin.shared.processed_blocks.fetch_add(HOST_GRACE_BLOCKS, Ordering::Relaxed);
        assert_eq!(shown_direction(&mut editor), "Forward", "the host dropped the edit, so the menu shows the real value");

        pick_direction(&mut editor, "Levitate");
        editor.wait(1.1);
        assert_one_edit(&editor.screen.take_edits(), direction, 1.0);
        assert_eq!(shown_direction(&mut editor), "Levitate");
    }

    /// Cubase's side of the editor's edits, as far as the audio goes: between editor frames it
    /// calls `process`, and a value the editor set reaches `process` only while the edit is still
    /// open. An edit opened and closed within one frame never arrives, which is why a single
    /// begin, set, and end did nothing in Cubase.
    #[derive(Default)]
    struct CubaseLike {
        /// The value of the edit that is open, if one is.
        open: Option<Option<f32>>,
        /// The values `process` received.
        received: Vec<f32>,
    }

    impl CubaseLike {
        fn hear(&mut self, edits: &[Edit], param: ParamPtr) {
            for edit in edits {
                match *edit {
                    Edit::Begin(p) if p == param => {
                        assert!(self.open.is_none(), "an edit opened twice");
                        self.open = Some(None);
                    }
                    Edit::Set(p, value) if p == param => {
                        let open = self.open.as_mut().expect("a value sent outside an edit");
                        *open = Some(value);
                    }
                    Edit::End(p) if p == param => self.open = None,
                    _ => {}
                }
            }
        }

        fn process(&mut self) {
            if let Some(Some(value)) = self.open {
                self.received.push(value);
            }
        }
    }

    #[test]
    fn a_direction_reaches_a_host_that_only_takes_edits_held_across_process_calls() {
        let mut editor = Editor::new(true);
        editor.settle();
        editor.screen.take_edits();
        let direction = editor.plugin.params.direction.as_ptr();
        let mut host = CubaseLike::default();

        pick_direction(&mut editor, "Backward");
        let mut frames = 0;
        loop {
            host.hear(&editor.screen.take_edits(), direction);
            if host.open.is_none() {
                break;
            }
            host.process();
            editor.plugin.shared.processed_blocks.fetch_add(1, Ordering::Relaxed);
            assert_eq!(shown_direction(&mut editor), "Backward", "frame {frames}");
            frames += 1;
            assert!(frames < 200, "the edit is closed in the end");
        }
        assert!(host.received.len() >= MIN_HOLD_FRAMES as usize - 2, "process heard Backward {} times", host.received.len());
        assert!(host.received.iter().all(|&value| value == 0.5), "only ever Backward: {:?}", host.received);

        // A choice the parameter already has closes as soon as the edit has been held for a click.
        pick_direction(&mut editor, "Forward");
        let mut frames = 0;
        loop {
            host.hear(&editor.screen.take_edits(), direction);
            if host.open.is_none() {
                break;
            }
            editor.frame();
            frames += 1;
        }
        assert!(frames <= MIN_HOLD_FRAMES as usize, "closed after {frames} more frames");
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
                        .show(context, |ui| knob_row(ui, setter, &[("Rate", &plugin.params.rate, turnable, Face::Plain)]).0)
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

    #[test]
    fn right_clicking_dream_eater_picks_its_key_without_turning_it_or_showing_it() {
        let mut editor = Editor::new(true);
        let row = editor.settle().lower_knobs;
        let dream_eater = knob_centres(row).last().unwrap();
        // Everything but the gain reduction readout, which falls while the test runs.
        let panel_texts = |screen: &Screen| {
            let mut texts: Vec<String> = screen.texts().into_iter().filter(|text| !text.starts_with(" -")).collect();
            texts.sort();
            texts
        };
        let panel = panel_texts(&editor.screen);
        let labels: Vec<String> = KeyChoice::all().map(|choice| choice.label()).collect();
        assert_eq!(labels.len(), 13);
        let named: Vec<&String> = panel.iter().filter(|text| labels.contains(text)).collect();
        assert_eq!(named, ["E"], "nothing on the panel names a key; the E is the End handle's tab");
        editor.screen.take_edits();

        editor.move_to(dream_eater);
        editor.screen.right_button(true);
        editor.frame();
        editor.screen.right_button(false);
        editor.settle();
        let shown = editor.screen.texts();
        let count = |texts: &[String], label: &String| texts.iter().filter(|text| *text == label).count();
        assert!(
            labels.iter().all(|label| count(&shown, label) == count(&panel, label) + 1),
            "Auto and the twelve keys, once each: {shown:?}"
        );
        assert_eq!(editor.plugin.params.dream_key.get(), KeyChoice::Auto);

        let d_major = editor.screen.find_text(&KeyChoice::Fixed(2).label()).unwrap();
        editor.click_at(d_major.center());
        editor.settle();
        assert_eq!(editor.plugin.params.dream_key.get(), KeyChoice::Fixed(2));
        assert_eq!(editor.screen.take_edits(), [], "the key is not a parameter, and the knob did not move");
        assert_eq!(editor.plugin.params.dream_eater.value(), 0.0);
        assert_eq!(panel_texts(&editor.screen), panel, "the menu closes and the panel looks the same");

        // A right-drag does not turn it either.
        editor.move_to(dream_eater);
        editor.screen.right_button(true);
        editor.frame();
        for step in 1..=4 {
            editor.screen.cursor_moved(dream_eater - egui::vec2(0.0, 10.0 * step as f32));
            editor.frame();
        }
        editor.screen.right_button(false);
        editor.frame();
        editor.screen.press_key(egui::Key::Escape);
        editor.settle();
        assert_eq!(editor.screen.take_edits(), []);
        assert_eq!(editor.plugin.params.dream_key.get(), KeyChoice::Fixed(2));
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
        assert_eq!(entries, ["--", "Daisy", "Paper", "Press", "Spin", "Stone", "Umber", "Water", "Wind"]);
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
        assert_eq!(params.preset.get().choice, Choice::None);
        assert_eq!(shown_preset(&mut editor), "--");
        editor.plugin.adopt_pending_sample();
        assert!(editor.plugin.engine.sample().is_none());

        crate::install_sample(another_sample(), &params, &editor.plugin.shared);
        assert_eq!(shown_preset(&mut editor), "--*", "a loaded sample changes --");
    }

    #[test]
    fn every_factory_preset_loads_its_own_sample() {
        let mut editor = Editor::new(false);
        editor.settle();
        for name in ["Daisy", "Paper", "Press", "Spin", "Stone", "Umber", "Water", "Wind"] {
            pick_preset(&mut editor, name);
            let params = editor.plugin.params.clone();
            assert_eq!(params.preset.get().choice, Choice::Factory(name.into()));
            let sample = params.sample.read(|s| s.cloned()).unwrap_or_else(|| panic!("{name} loads a sample"));
            assert_eq!(sample, crate::preset::Preset::factory(name).unwrap().sample.unwrap(), "{name}");
            assert_eq!(shown_preset(&mut editor), name);
        }
    }

    #[test]
    fn choosing_a_user_preset_sends_its_settings_and_loads_its_sample() {
        let mut editor = Editor::new(false);
        editor.settle();
        let mut preset =
            crate::preset::Preset { sample: Some(another_sample()), dream_key: KeyChoice::Fixed(7), ..Default::default() };
        for (id, value) in [("rate", 4.0), ("restart", 0.0), ("direction", 1.0), ("start", 0.125), ("depth", 0.4), ("release", 321.5)] {
            preset.values.insert(id.to_owned(), value);
        }
        editor.plugin.shared.user_presets.save("Snare Gate", &preset).unwrap();
        let entries: Vec<String> = open_preset_menu(&mut editor).into_iter().map(|(text, _)| text).collect();
        assert_eq!(
            entries,
            ["--", "Daisy", "Paper", "Press", "Spin", "Stone", "Umber", "Water", "Wind", "Snare Gate"],
            "user presets come after the factory ones"
        );
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
        assert_eq!(params.dream_key.get(), KeyChoice::Fixed(7), "the preset's Dream Eater key comes with it");

        pick_preset(&mut editor, "Water");
        assert_eq!(params.dream_key.get(), KeyChoice::Auto, "factory presets listen for the key");
        params.dream_key.set(KeyChoice::Fixed(3));
        pick_preset(&mut editor, "--");
        assert_eq!(params.dream_key.get(), KeyChoice::Auto);
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
        for (id, value) in [("depth", 0.3), ("restart", 0.0), ("direction", 2.0), ("start", 0.2)] {
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
