use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use nih_plug::wrapper::state::{ParamValue, PluginState};
use nih_plug_egui::EguiState;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub mod audio_file;
pub mod dream;
pub mod drive;
pub mod dsp;
mod editor;
pub mod preset;
pub mod sample;

use dream::KeyChoice;
use dsp::{Direction, Engine, FrameGains, Settings, SyncRate, TransportInfo, HP_OFF_HZ, LP_OFF_HZ};
use preset::{Choice, Preset, Selection, SelectionSlot, UserPresets};
use sample::{EmbeddedSample, Overview, PreparedSample};

/// The persisted sample. Every replacement, including one from a loaded project or preset, bumps
/// the generation so the plugin knows to rebuild its playback copy.
#[derive(Default)]
pub struct SampleSlot {
    sample: RwLock<Option<EmbeddedSample>>,
    generation: AtomicU64,
    /// [`EmbeddedSample::fingerprint`] of the sample, or 0 for none.
    fingerprint: AtomicU64,
}

impl SampleSlot {
    /// Stores a new sample and returns its generation.
    pub fn replace(&self, sample: Option<EmbeddedSample>) -> u64 {
        let fingerprint = sample.as_ref().map_or(0, EmbeddedSample::fingerprint);
        *self.sample.write().unwrap() = sample;
        self.fingerprint.store(fingerprint, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint.load(Ordering::Acquire)
    }

    pub fn read<R>(&self, f: impl FnOnce(Option<&EmbeddedSample>) -> R) -> R {
        f(self.sample.read().unwrap().as_ref())
    }
}

impl<'a> PersistentField<'a, Option<EmbeddedSample>> for SampleSlot {
    fn set(&self, new_value: Option<EmbeddedSample>) {
        self.replace(new_value);
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Option<EmbeddedSample>) -> R,
    {
        f(&self.sample.read().unwrap())
    }
}

/// The persisted key Dream Eater tunes to, readable from the audio thread without a lock.
pub struct KeyChoiceSlot(AtomicU8);

impl Default for KeyChoiceSlot {
    fn default() -> Self {
        KeyChoiceSlot(AtomicU8::new(KeyChoice::Auto.to_bits()))
    }
}

impl KeyChoiceSlot {
    pub fn get(&self) -> KeyChoice {
        KeyChoice::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, choice: KeyChoice) {
        self.0.store(choice.to_bits(), Ordering::Relaxed);
    }
}

impl<'a> PersistentField<'a, KeyChoice> for KeyChoiceSlot {
    fn set(&self, new_value: KeyChoice) {
        KeyChoiceSlot::set(self, new_value);
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&KeyChoice) -> R,
    {
        f(&self.get())
    }
}

struct PendingSample {
    generation: u64,
    /// `None` clears the sample.
    prepared: Option<Arc<PreparedSample>>,
}

/// State shared by the audio thread, the editor, and the file loader.
pub struct Shared {
    host_rate: AtomicF32,
    /// Length of the loaded sample, for the Start and End readouts.
    sample_seconds: Arc<AtomicF32>,
    /// A freshly loaded sample waiting for the audio thread.
    pending: Mutex<Option<PendingSample>>,
    /// The sample the audio thread swapped out, freed later off the audio thread.
    retired: Mutex<Option<Arc<PreparedSample>>>,
    overview: Mutex<Option<Arc<Overview>>>,
    status: Mutex<String>,
    loading: AtomicBool,
    /// Set by the loader so the editor resets Start and End for the new sample.
    reset_region: AtomicBool,
    /// Largest gain reduction in dB since the editor last looked.
    gain_reduction_db: AtomicF32,
    /// Playback position as a fraction of the sample, or negative when idle.
    playhead: AtomicF32,
    /// 0..1, how far the sample is sweeping the Dream Eater comb right now.
    dream_level: AtomicF32,
    /// Blocks processed so far. While the host runs the plugin, an edit from the editor only
    /// reaches the parameter in a later block, so this tells the editor whether it has had one.
    processed_blocks: AtomicU64,
    user_presets: UserPresets,
}

impl Shared {
    fn new(user_presets: UserPresets) -> Self {
        Shared {
            host_rate: AtomicF32::new(48_000.0),
            sample_seconds: Arc::new(AtomicF32::new(0.0)),
            pending: Mutex::new(None),
            retired: Mutex::new(None),
            overview: Mutex::new(None),
            status: Mutex::new(String::new()),
            loading: AtomicBool::new(false),
            reset_region: AtomicBool::new(false),
            gain_reduction_db: AtomicF32::new(0.0),
            playhead: AtomicF32::new(-1.0),
            dream_level: AtomicF32::new(0.0),
            processed_blocks: AtomicU64::new(0),
            user_presets,
        }
    }

    fn show_sample(&self, sample: Option<&EmbeddedSample>) {
        self.sample_seconds.store(sample.map_or(0.0, |s| s.seconds() as f32), Ordering::Relaxed);
        *self.overview.lock().unwrap() = sample.map(|s| Arc::new(Overview::new(s, 1024)));
    }

    fn set_status(&self, message: impl Into<String>) {
        *self.status.lock().unwrap() = message.into();
    }
}

/// Decodes and installs a WAV, AIFF, or MP3 on a background thread.
pub fn load_file(path: PathBuf, params: Arc<HaunterParams>, shared: Arc<Shared>) {
    if shared.loading.swap(true, Ordering::AcqRel) {
        return;
    }
    shared.set_status("Loading…");
    std::thread::spawn(move || {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sample".to_owned());
        match audio_file::decode_file(&path) {
            Ok(audio) => match EmbeddedSample::from_decoded(name, audio) {
                Some(sample) => {
                    install_sample(sample, &params, &shared);
                    shared.set_status("");
                }
                None => shared.set_status("That file has no audio in it"),
            },
            Err(err) => shared.set_status(err.to_string()),
        }
        shared.loading.store(false, Ordering::Release);
    });
}

/// Installs a newly loaded sample, and has the editor reset Start and End for it.
pub fn install_sample(sample: EmbeddedSample, params: &HaunterParams, shared: &Shared) {
    install(Some(sample), params, shared);
    shared.reset_region.store(true, Ordering::Release);
}

/// Installs or clears the sample, leaving Start and End alone.
fn install(sample: Option<EmbeddedSample>, params: &HaunterParams, shared: &Shared) {
    let host_rate = shared.host_rate.load(Ordering::Acquire) as f64;
    let prepared = sample.as_ref().map(|sample| Arc::new(PreparedSample::new(sample, host_rate)));
    shared.show_sample(sample.as_ref());
    let generation = params.sample.replace(sample);
    shared.retired.lock().unwrap().take();
    *shared.pending.lock().unwrap() = Some(PendingSample { generation, prepared });
}

/// Loads `preset`: sends the host an edit for every parameter, the same way a knob does, and
/// installs its sample. Remembers what it set so the editor can tell when something changes.
pub fn apply_preset(choice: Choice, preset: Preset, setter: &ParamSetter, params: &HaunterParams, shared: &Shared) {
    for (id, ptr, _) in params.param_map() {
        let normalized = unsafe { ptr.preview_normalized(preset.value(&id, &ptr)) };
        unsafe {
            setter.raw_context.raw_begin_set_parameter(ptr);
            setter.raw_context.raw_set_parameter_normalized(ptr, normalized);
            setter.raw_context.raw_end_set_parameter(ptr);
        }
    }
    let values = preset.complete_values(params);
    let sample = preset.sample.as_ref().map_or(0, EmbeddedSample::fingerprint);
    params.dream_key.set(preset.dream_key);
    install(preset.sample, params, shared);
    params.preset.set(Selection { choice, values, sample });
    shared.set_status("");
}

/// Saves the sample and every parameter as the user preset `name`, and selects it. Returns the
/// name as saved.
pub fn save_preset(name: &str, params: &HaunterParams, shared: &Shared) -> Result<String, String> {
    let preset = Preset::capture(params, params.sample.read(|sample| sample.cloned()), params.dream_key.get());
    let name = shared.user_presets.save(name, &preset)?;
    let sample = preset.sample.as_ref().map_or(0, EmbeddedSample::fingerprint);
    params.preset.set(Selection { choice: Choice::User(name.clone()), values: preset.values, sample });
    Ok(name)
}

/// Reads the preset `choice` names, from the plugin or the user's folder.
pub fn read_preset(choice: &Choice, shared: &Shared) -> Result<Preset, String> {
    match choice {
        Choice::None => Ok(Preset::default()),
        Choice::Factory(name) => Preset::factory(name),
        Choice::User(name) => shared.user_presets.load(name),
    }
}

#[derive(Params)]
pub struct HaunterParams {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    #[persist = "sample"]
    pub sample: SampleSlot,

    /// The selected preset, so a project reopens showing it.
    #[persist = "preset"]
    pub preset: SelectionSlot,

    /// The key Dream Eater tunes to, chosen from the knob's right-click menu.
    #[persist = "dream-key"]
    pub dream_key: KeyChoiceSlot,

    #[id = "rate"]
    pub rate: EnumParam<SyncRate>,
    #[id = "restart"]
    pub restart: BoolParam,
    /// Replaced the Reverse toggle (ID `reverse`) in 1.7. A new ID, since a toggle's saved value
    /// and automation don't fit three choices; old states map Reverse onto it in `migrate_state`.
    #[id = "direction"]
    pub direction: EnumParam<Direction>,
    #[id = "start"]
    pub start: FloatParam,
    #[id = "end"]
    pub end: FloatParam,

    #[id = "depth"]
    pub depth: FloatParam,
    #[id = "attack"]
    pub attack: FloatParam,
    #[id = "release"]
    pub release: FloatParam,

    #[id = "hp"]
    pub high_pass: FloatParam,
    #[id = "lp"]
    pub low_pass: FloatParam,

    #[id = "drive"]
    pub drive: FloatParam,
    #[id = "blend"]
    pub blend: FloatParam,

    #[id = "mix"]
    pub mix: FloatParam,
    #[id = "dream-eater"]
    pub dream_eater: FloatParam,
}

fn region_param(name: &str, default: f32, seconds: Arc<AtomicF32>) -> FloatParam {
    let display = seconds.clone();
    FloatParam::new(name, default, FloatRange::Linear { min: 0.0, max: 1.0 })
        .with_value_to_string(Arc::new(move |value| {
            let length = display.load(Ordering::Relaxed);
            if length > 0.0 {
                format!("{:.0} ms", value * length * 1000.0)
            } else {
                format!("{:.0}%", value * 100.0)
            }
        }))
        .with_string_to_value(Arc::new(move |text| {
            let text = text.trim();
            if let Some(percent) = text.strip_suffix('%') {
                return percent.trim().parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0));
            }
            let ms = text.trim_end_matches("ms").trim().parse::<f32>().ok()?;
            let length = seconds.load(Ordering::Relaxed);
            (length > 0.0).then(|| (ms / 1000.0 / length).clamp(0.0, 1.0))
        }))
}

fn percent_param(name: &str, default: f32) -> FloatParam {
    FloatParam::new(name, default, FloatRange::Linear { min: 0.0, max: 1.0 })
        .with_smoother(SmoothingStyle::Linear(20.0))
        .with_unit("%")
        .with_value_to_string(formatters::v2s_f32_percentage(0))
        .with_string_to_value(formatters::s2v_f32_percentage())
}

fn time_param(name: &str, default: f32, min: f32, max: f32) -> FloatParam {
    FloatParam::new(name, default, FloatRange::Skewed { min, max, factor: FloatRange::skew_factor(-2.0) })
        .with_unit(" ms")
        .with_value_to_string(Arc::new(|value| {
            if value < 10.0 {
                format!("{value:.1}")
            } else {
                format!("{value:.0}")
            }
        }))
}

fn cutoff_param(name: &str, default: f32, min: f32, max: f32, off_at: f32) -> FloatParam {
    let hz = formatters::v2s_f32_hz_then_khz(1);
    FloatParam::new(name, default, FloatRange::Skewed { min, max, factor: FloatRange::skew_factor(-2.0) })
        .with_value_to_string(Arc::new(move |value| {
            if (value - off_at).abs() < 0.5 {
                "Off".to_owned()
            } else {
                hz(value)
            }
        }))
        .with_string_to_value(formatters::s2v_f32_hz_then_khz())
}

/// Whole-percent steps, so the knob is either exactly Off or at least 1%.
fn dream_eater_param() -> FloatParam {
    FloatParam::new("Dream Eater", 0.0, FloatRange::Linear { min: 0.0, max: 1.0 })
        .with_step_size(0.01)
        .with_smoother(SmoothingStyle::Linear(50.0))
        .with_value_to_string(Arc::new(|value| {
            if value <= 0.0 {
                "Off".to_owned()
            } else {
                format!("{:.0}%", value * 100.0)
            }
        }))
        .with_string_to_value(Arc::new(|text| {
            let text = text.trim();
            if text.eq_ignore_ascii_case("off") {
                return Some(0.0);
            }
            text.trim_end_matches('%').trim().parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0))
        }))
}

impl HaunterParams {
    fn new(sample_seconds: Arc<AtomicF32>) -> Self {
        HaunterParams {
            editor_state: EguiState::from_size(editor::WIDTH, editor::HEIGHT),
            sample: SampleSlot::default(),
            preset: SelectionSlot::default(),
            dream_key: KeyChoiceSlot::default(),
            rate: EnumParam::new("Rate", SyncRate::Quarter),
            restart: BoolParam::new("Restart", true),
            direction: EnumParam::new("Direction", Direction::Forward),
            start: region_param("Start", 0.0, sample_seconds.clone()),
            end: region_param("End", 1.0, sample_seconds),
            depth: percent_param("Depth", 0.75),
            attack: time_param("Attack", 1.0, 0.1, 100.0),
            release: time_param("Release", 80.0, 5.0, 1000.0),
            high_pass: cutoff_param("HP", HP_OFF_HZ, HP_OFF_HZ, 2_000.0, HP_OFF_HZ),
            low_pass: cutoff_param("LP", LP_OFF_HZ, 50.0, LP_OFF_HZ, LP_OFF_HZ),
            drive: percent_param("Drive", 0.0),
            blend: percent_param("Sample Blend", 0.0),
            mix: percent_param("Mix", 1.0),
            dream_eater: dream_eater_param(),
        }
    }

    fn settings(&self) -> Settings {
        Settings {
            rate: self.rate.value(),
            restart: self.restart.value(),
            direction: self.direction.value(),
            start: self.start.value(),
            end: self.end.value(),
            attack_ms: self.attack.value(),
            release_ms: self.release.value(),
            high_pass_hz: self.high_pass.value(),
            low_pass_hz: self.low_pass.value(),
            dream_key: self.dream_key.get(),
        }
    }
}

/// Brings older states up to date. From 1.0, One-shot mode becomes Restart on and Loop mode
/// becomes Restart off, and Invert is dropped. Up to 1.1, the Output gain is dropped, because the
/// output is now always at unity. A state without Dream Eater loads with it Off, and one without
/// a Dream Eater key loads on Auto, since whatever a state leaves out would otherwise keep the
/// value it had. Up to 1.6, Reverse on becomes Direction Backward and off becomes Forward, and a
/// state with neither loads on Forward. Up to 1.7.11, Sample Volume is folded into Sample Blend,
/// capped at 100%, and Drive, which took its place, loads at 0.
fn migrate_state(state: &mut PluginState) {
    if let Some(reverse) = state.params.remove("reverse") {
        let backward = matches!(reverse, ParamValue::Bool(true))
            || matches!(reverse, ParamValue::F32(v) if v >= 0.5)
            || matches!(reverse, ParamValue::I32(v) if v != 0);
        let direction = if backward { "backward" } else { "forward" };
        state.params.entry("direction".to_owned()).or_insert(ParamValue::String(direction.to_owned()));
    }
    state.params.entry("direction".to_owned()).or_insert(ParamValue::String("forward".to_owned()));
    if let Some(mode) = state.params.remove("mode") {
        let looped = matches!(&mode, ParamValue::String(id) if id == "loop") || matches!(mode, ParamValue::I32(1));
        state.params.entry("restart".to_owned()).or_insert(ParamValue::Bool(!looped));
    }
    state.params.remove("invert");
    state.params.remove("output");
    if let Some(ParamValue::F32(volume)) = state.params.remove("sample-volume") {
        let blend = match state.params.get("blend") {
            Some(ParamValue::F32(blend)) => *blend,
            _ => 0.0,
        };
        state.params.insert("blend".to_owned(), ParamValue::F32(preset::fold_sample_volume(blend, volume)));
    }
    state.params.entry("drive".to_owned()).or_insert(ParamValue::F32(0.0));
    state.params.entry("dream-eater".to_owned()).or_insert(ParamValue::F32(0.0));
    state
        .fields
        .entry("dream-key".to_owned())
        .or_insert_with(|| serde_json::to_string(&KeyChoice::Auto).unwrap());
}

pub struct Haunter {
    params: Arc<HaunterParams>,
    shared: Arc<Shared>,
    engine: Engine,
    /// The sample generation the engine is playing.
    engine_generation: u64,
}

impl Default for Haunter {
    fn default() -> Self {
        let plugin = Haunter::with_preset_dir(UserPresets::default_dir());
        #[cfg(feature = "standalone")]
        if let Some(path) = std::env::var_os("HAUNTER_PREVIEW_SAMPLE") {
            load_file(path.into(), plugin.params.clone(), plugin.shared.clone());
        }
        plugin
    }
}

impl Haunter {
    /// A plugin that keeps its user presets in `dir`.
    pub fn with_preset_dir(dir: PathBuf) -> Self {
        let shared = Arc::new(Shared::new(UserPresets::new(dir)));
        let params = Arc::new(HaunterParams::new(shared.sample_seconds.clone()));
        Haunter {
            params,
            shared,
            engine: Engine::new(48_000.0),
            engine_generation: 0,
        }
    }

    /// Rebuilds the playback copy from the persisted sample if it or the host rate changed.
    fn sync_sample(&mut self, sample_rate: f64) {
        let generation = self.params.sample.generation();
        let rate_changed = self.engine.sample().is_some_and(|s| s.sample_rate != sample_rate);
        if generation != self.engine_generation || rate_changed {
            self.shared.pending.lock().unwrap().take();
            let prepared = self.params.sample.read(|sample| {
                self.shared.show_sample(sample);
                sample.map(|s| Arc::new(PreparedSample::new(s, sample_rate)))
            });
            self.engine.set_sample(prepared);
            self.engine_generation = generation;
        }
    }

    /// Picks up a sample from the loader without blocking or freeing memory on the audio thread.
    fn adopt_pending_sample(&mut self) {
        let Ok(mut pending) = self.shared.pending.try_lock() else {
            return;
        };
        let Some(next) = pending.take() else {
            return;
        };
        drop(pending);
        if next.prepared.as_ref().is_some_and(|prepared| prepared.sample_rate != self.engine.sample_rate()) {
            return;
        }
        let old = self.engine.set_sample(next.prepared);
        self.engine_generation = next.generation;
        if let (Some(old), Ok(mut retired)) = (old, self.shared.retired.try_lock()) {
            if retired.is_none() {
                *retired = Some(old);
            }
        }
    }
}

impl Plugin for Haunter {
    const NAME: &'static str = "Haunter";
    const VENDOR: &'static str = "Haunter";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn filter_state(state: &mut PluginState) {
        migrate_state(state);
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.shared.clone(), self.params.editor_state.clone())
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        let sample_rate = buffer_config.sample_rate as f64;
        self.shared.host_rate.store(buffer_config.sample_rate, Ordering::Release);
        self.engine.prepare(sample_rate);
        self.sync_sample(sample_rate);
        self.engine.settings = self.params.settings();
        self.engine.finish_analysis();
        true
    }

    fn reset(&mut self) {
        self.engine.reset();
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        self.shared.processed_blocks.fetch_add(1, Ordering::Relaxed);
        self.adopt_pending_sample();
        self.engine.settings = self.params.settings();

        let transport = context.transport();
        let sample_rate = self.engine.sample_rate();
        let info = TransportInfo {
            playing: transport.playing,
            beat: transport.pos_beats(),
            beats_per_frame: transport.tempo.map(|tempo| tempo / 60.0 / sample_rate),
            cycle: transport.loop_range_beats(),
        };

        let params = &self.params;
        let report = self.engine.process(buffer.as_slice(), &info, || {
            FrameGains {
                depth: params.depth.smoothed.next(),
                mix: params.mix.smoothed.next(),
                sample_level: params.blend.smoothed.next(),
                dream_eater: params.dream_eater.smoothed.next(),
                drive: params.drive.smoothed.next(),
            }
        });

        if params.editor_state.is_open() {
            let reduction = -util::gain_to_db(report.min_track_gain.max(1e-6));
            let meter = &self.shared.gain_reduction_db;
            if reduction > meter.load(Ordering::Relaxed) {
                meter.store(reduction, Ordering::Relaxed);
            }
            self.shared.playhead.store(report.playhead.unwrap_or(-1.0), Ordering::Relaxed);
            self.shared.dream_level.store(report.dream_level, Ordering::Relaxed);
        }

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for Haunter {
    // Never change these bytes: hosts find the plugin in saved projects by this ID.
    const VST3_CLASS_ID: [u8; 16] = [
        0x47, 0x68, 0x6F, 0x73, 0x74, 0x70, 0x75, 0x6D, 0x70, 0x44, 0x75, 0x63, 0x6B, 0x46, 0x78, 0x31,
    ];
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Dynamics];
}

nih_export_vst3!(Haunter);

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fresh folder under the system temp folder, deleted when dropped.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let unique = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
            TempDir(std::env::temp_dir().join("haunter-tests").join(unique))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use test_support::TempDir;

    fn kick(rate: f64) -> EmbeddedSample {
        let frames = (0.3 * rate) as usize;
        let left: Vec<f32> = (0..frames)
            .map(|n| {
                let t = n as f64 / rate;
                ((2.0 * std::f64::consts::PI * 55.0 * t).sin() * (-t * 18.0).exp()) as f32
            })
            .collect();
        let right = left.iter().map(|v| v * 0.5).collect();
        EmbeddedSample { name: "ghost kick.wav".into(), sample_rate: rate, channels: vec![left, right] }
    }

    /// Plays two beats at 120 bpm through the plugin's engine and returns the left output for a
    /// track held at 1.0 with the sample blended in.
    fn render(plugin: &mut Haunter, host_rate: f64) -> Vec<f32> {
        plugin.engine.prepare(host_rate);
        plugin.sync_sample(host_rate);
        plugin.engine.finish_analysis();
        let frames = host_rate as usize;
        let mut left = vec![1.0f32; frames];
        let mut right = vec![1.0f32; frames];
        let gains = FrameGains { depth: 0.8, mix: 1.0, sample_level: 0.5, dream_eater: 0.0, drive: 0.0 };
        plugin.engine.process(
            &mut [&mut left[..], &mut right[..]],
            &TransportInfo::playing_at(0.0, 120.0, host_rate),
            || gains,
        );
        left
    }

    /// The non-parameter part of the plugin state, serialized the way nih-plug's wrapper does it:
    /// each persisted field as a JSON string, inside the JSON state object.
    fn save_state(plugin: &Haunter) -> String {
        serde_json::to_string(&plugin.params.serialize_fields()).unwrap()
    }

    fn load_state(plugin: &Haunter, state: &str) {
        let fields: BTreeMap<String, String> = serde_json::from_str(state).unwrap();
        plugin.params.deserialize_fields(&fields);
    }

    #[test]
    fn state_embeds_the_sample_and_restores_it() {
        let mut original = Haunter::default();
        install_sample(kick(44_100.0), &original.params, &original.shared);
        let state = save_state(&original);
        assert!(state.contains("ghost kick.wav"));

        let mut restored = Haunter::default();
        assert!(restored.params.sample.read(|s| s.is_none()));
        load_state(&restored, &state);

        let before = original.params.sample.read(|s| s.cloned()).unwrap();
        let after = restored.params.sample.read(|s| s.cloned()).unwrap();
        assert_eq!(before, after, "the audio itself comes back bit for bit");

        for plugin in [&mut original, &mut restored] {
            plugin.engine.settings.start = 0.1;
            plugin.engine.settings.restart = false;
        }
        let original_output = render(&mut original, 48_000.0);
        assert_eq!(original_output, render(&mut restored, 48_000.0));
        assert!(original_output.iter().any(|&v| v < 0.5));
    }

    #[test]
    fn a_loaded_mp3_is_embedded_in_the_state_like_a_wav() {
        let folder = TempDir::new();
        std::fs::create_dir_all(folder.path()).unwrap();
        let path = folder.path().join("ghost tone.mp3");
        std::fs::write(&path, include_bytes!("../tests/fixtures/tone-1khz.mp3")).unwrap();

        let mut original = Haunter::default();
        load_file(path.clone(), original.params.clone(), original.shared.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while original.shared.loading.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(*original.shared.status.lock().unwrap(), "", "loaded without an error");
        let loaded = original.params.sample.read(|s| s.cloned()).expect("the MP3 is the sample");
        assert_eq!(loaded.name, "ghost tone.mp3");
        assert_eq!((loaded.sample_rate, loaded.channels.len(), loaded.frames()), (44_100.0, 2, 11_025));

        let state = save_state(&original);
        std::fs::remove_file(&path).unwrap();
        let mut restored = Haunter::default();
        load_state(&restored, &state);
        assert_eq!(restored.params.sample.read(|s| s.cloned()), Some(loaded), "decoded audio comes back bit for bit");
        assert_eq!(render(&mut original, 48_000.0), render(&mut restored, 48_000.0));
        assert!(restored.engine.sample().is_some_and(|sample| sample.frames() == 12_000), "resampled to the host rate");
    }

    #[test]
    fn parameter_ids_are_stable() {
        let plugin = Haunter::default();
        let ids: Vec<String> = plugin.params.param_map().into_iter().map(|(id, _, _)| id).collect();
        for id in [
            "rate", "restart", "direction", "start", "end", "depth", "attack", "release", "hp", "lp",
            "drive", "blend", "mix", "dream-eater",
        ] {
            assert!(ids.iter().any(|known| known == id), "missing parameter {id}");
        }
        assert!(!ids.iter().any(|id| id == "mode" || id == "invert" || id == "output" || id == "reverse"));
        assert!(!ids.iter().any(|id| id == "sample-volume"), "Drive took Sample Volume's place");
        let fields = plugin.params.serialize_fields();
        assert_eq!(fields.get("sample").map(String::as_str), Some("null"));
        assert!(fields.contains_key("editor-state"));
        assert_eq!(fields.get("dream-key").map(String::as_str), Some(r#""auto""#));
        assert_eq!(fields.get("preset").map(String::as_str), Some(r#"{"choice":{"kind":"none"},"values":{},"sample":0}"#));
    }

    #[test]
    fn a_saved_user_preset_round_trips_the_sample_and_every_parameter() {
        let folder = TempDir::new();
        let plugin = Haunter::with_preset_dir(folder.path().join("Presets"));
        assert!(plugin.shared.user_presets.list().is_empty(), "no folder yet means no presets");
        install_sample(kick(44_100.0), &plugin.params, &plugin.shared);

        let name = save_preset("  Ghost Kick ", &plugin.params, &plugin.shared).unwrap();
        assert_eq!(name, "Ghost Kick");
        assert!(folder.path().join("Presets").join("Ghost Kick.json").is_file());
        assert_eq!(plugin.shared.user_presets.list(), ["Ghost Kick"]);
        let selection = plugin.params.preset.get();
        assert_eq!(selection.choice, Choice::User("Ghost Kick".into()));
        assert!(!selection.is_modified(plugin.params.as_ref(), plugin.params.sample.fingerprint(), false));

        let loaded = read_preset(&selection.choice, &plugin.shared).unwrap();
        let ids: Vec<String> = plugin.params.param_map().into_iter().map(|(id, _, _)| id).collect();
        assert!(ids.iter().all(|id| loaded.values.contains_key(id)), "every parameter is saved");
        assert_eq!(loaded.values, selection.values);
        assert_eq!(loaded.sample, plugin.params.sample.read(|s| s.cloned()), "the audio comes back bit for bit");

        // Values away from the defaults survive too, and saving under the same name replaces it.
        let mut custom = loaded.clone();
        for (id, value) in [("rate", 4.0), ("restart", 0.0), ("direction", 2.0), ("start", 0.125), ("end", 0.75), ("depth", 0.4)] {
            custom.values.insert(id.to_owned(), value);
        }
        custom.values.insert("release".to_owned(), 321.5);
        custom.values.insert("dream-eater".to_owned(), 0.37);
        plugin.shared.user_presets.save("ghost kick", &custom).unwrap();
        assert_eq!(plugin.shared.user_presets.list().len(), 1, "one preset per name");
        assert_eq!(read_preset(&Choice::User("Ghost Kick".into()), &plugin.shared).unwrap(), custom);

        let elsewhere = Haunter::with_preset_dir(folder.path().join("Presets"));
        assert_eq!(elsewhere.shared.user_presets.list().len(), 1, "presets outlive the plugin instance");
        assert!(save_preset("Water", &plugin.params, &plugin.shared).is_err(), "factory names are taken");
        assert!(save_preset("a/b", &plugin.params, &plugin.shared).is_err());
        assert!(!folder.path().join("Presets").read_dir().unwrap().any(|e| e.unwrap().path().extension().unwrap() == "tmp"));
    }

    #[test]
    fn damaged_or_missing_presets_report_an_error() {
        let folder = TempDir::new();
        let plugin = Haunter::with_preset_dir(folder.path().to_owned());
        assert!(read_preset(&Choice::User("Nope".into()), &plugin.shared).is_err());
        std::fs::create_dir_all(folder.path()).unwrap();
        std::fs::write(folder.path().join("Broken.json"), "{ not json").unwrap();
        assert_eq!(plugin.shared.user_presets.list(), ["Broken"]);
        let error = read_preset(&Choice::User("Broken".into()), &plugin.shared).unwrap_err();
        assert!(error.contains("damaged"), "{error}");
    }

    #[test]
    fn the_selected_preset_is_kept_in_the_plugin_state() {
        let folder = TempDir::new();
        let original = Haunter::with_preset_dir(folder.path().to_owned());
        install_sample(kick(44_100.0), &original.params, &original.shared);
        save_preset("Mine", &original.params, &original.shared).unwrap();
        install_sample(kick(48_000.0), &original.params, &original.shared);

        let restored = Haunter::with_preset_dir(folder.path().to_owned());
        load_state(&restored, &save_state(&original));
        let selection = restored.params.preset.get();
        assert_eq!(selection, original.params.preset.get());
        assert_eq!(selection.choice.name(), "Mine");
        assert!(
            selection.is_modified(restored.params.as_ref(), restored.params.sample.fingerprint(), false),
            "the project reopens showing the preset as changed, because the sample was replaced"
        );
    }

    #[test]
    fn the_dream_eater_key_is_kept_in_the_plugin_state() {
        let original = Haunter::default();
        assert_eq!(original.params.dream_key.get(), KeyChoice::Auto, "Auto by default");
        assert_eq!(original.params.serialize_fields().get("dream-key").map(String::as_str), Some(r#""auto""#));
        original.params.dream_key.set(KeyChoice::Fixed(9));
        let state = save_state(&original);
        assert!(state.contains(r#""dream-key":"\"a\"""#), "{state}");
        let restored = Haunter::default();
        load_state(&restored, &state);
        assert_eq!(restored.params.dream_key.get(), KeyChoice::Fixed(9));
        assert_eq!(restored.params.settings().dream_key, KeyChoice::Fixed(9), "and the engine gets it");

        // A project from before 1.6 has no key, and opens on Auto even in an instance set to one.
        let mut fields = Haunter::default().params.serialize_fields();
        fields.remove("dream-key");
        let mut old = PluginState { version: "1.5.0".into(), params: BTreeMap::new(), fields };
        Haunter::filter_state(&mut old);
        restored.params.deserialize_fields(&old.fields);
        assert_eq!(restored.params.dream_key.get(), KeyChoice::Auto);
    }

    #[test]
    fn a_saved_user_preset_keeps_the_dream_eater_key() {
        let folder = TempDir::new();
        let plugin = Haunter::with_preset_dir(folder.path().to_owned());
        plugin.params.dream_key.set(KeyChoice::Fixed(4));
        save_preset("Bright", &plugin.params, &plugin.shared).unwrap();
        let loaded = read_preset(&Choice::User("Bright".into()), &plugin.shared).unwrap();
        assert_eq!(loaded.dream_key, KeyChoice::Fixed(4));
        let file = std::fs::read_to_string(folder.path().join("Bright.json")).unwrap();
        assert!(file.contains(r#""dream_key": "e""#), "{file}");

        // Presets saved before 1.6 have no key and listen for it.
        let old = file.replace(r#""dream_key": "e""#, r#""ignored": 0"#);
        std::fs::write(folder.path().join("Old.json"), old).unwrap();
        assert_eq!(read_preset(&Choice::User("Old".into()), &plugin.shared).unwrap().dream_key, KeyChoice::Auto);
        assert_eq!(read_preset(&Choice::Factory("Water".into()), &plugin.shared).unwrap().dream_key, KeyChoice::Auto);
    }

    #[test]
    fn projects_saved_with_water_or_no_preset_reopen_the_same() {
        let water = Preset::factory("Water").unwrap().sample.unwrap();
        let donor = Haunter::default();
        install_sample(water.clone(), &donor.params, &donor.shared);
        let mut fields = donor.params.serialize_fields();
        // The preset field exactly as 1.3 wrote it.
        let selection = format!(r#"{{"choice":{{"kind":"factory","name":"Water"}},"values":{{}},"sample":{}}}"#, water.fingerprint());
        fields.insert("preset".to_owned(), selection);
        let restored = Haunter::default();
        restored.params.deserialize_fields(&fields);
        let selection = restored.params.preset.get();
        assert_eq!(selection.choice, Choice::Factory("Water".into()));
        assert_eq!(selection.choice.name(), "Water");
        assert!(!selection.is_modified(restored.params.as_ref(), restored.params.sample.fingerprint(), false));
        assert_eq!(read_preset(&selection.choice, &restored.shared).unwrap().sample, Some(water));

        let mut fields = Haunter::default().params.serialize_fields();
        fields.insert("preset".to_owned(), r#"{"choice":{"kind":"none"},"values":{},"sample":0}"#.to_owned());
        let restored = Haunter::default();
        restored.params.deserialize_fields(&fields);
        let selection = restored.params.preset.get();
        assert_eq!(selection.choice, Choice::None);
        assert_eq!(selection.choice.name(), "--", "what 1.4 showed as Init, and 1.3 as --");
        assert!(!selection.is_modified(restored.params.as_ref(), restored.params.sample.fingerprint(), false));
    }

    #[test]
    fn projects_saved_on_the_removed_gaze_preset_reopen_with_their_sample() {
        // Gaze was a factory preset until 1.7.6. Its sample is in the project state, so a project
        // saved on it still plays it and shows "Gaze", unchanged, though the menu no longer lists it.
        let sample = Preset::factory("Water").unwrap().sample.unwrap();
        let gaze = EmbeddedSample { name: "Gaze".to_owned(), ..sample };
        let donor = Haunter::default();
        install_sample(gaze.clone(), &donor.params, &donor.shared);
        let mut fields = donor.params.serialize_fields();
        let selection = format!(r#"{{"choice":{{"kind":"factory","name":"Gaze"}},"values":{{}},"sample":{}}}"#, gaze.fingerprint());
        fields.insert("preset".to_owned(), selection);
        let restored = Haunter::default();
        restored.params.deserialize_fields(&fields);
        let selection = restored.params.preset.get();
        assert_eq!(selection.choice, Choice::Factory("Gaze".into()));
        assert_eq!(selection.choice.name(), "Gaze");
        assert!(!selection.is_modified(restored.params.as_ref(), restored.params.sample.fingerprint(), false));
        assert_eq!(restored.params.sample.read(|s| s.cloned()), Some(gaze));
        assert!(!crate::preset::factory_names().any(|name| name == "Gaze"));
        // Choosing it again isn't possible from the menu, and reading it fails cleanly.
        assert!(read_preset(&selection.choice, &restored.shared).is_err());
    }

    #[test]
    fn defaults() {
        let params = &Haunter::default().params;
        let drive = &params.drive;
        assert_eq!(drive.default_plain_value(), 0.0);
        assert_eq!(drive.to_string(), "0%");
        // What the host shows for the default it was given.
        assert_eq!(drive.normalized_value_to_string(drive.default_normalized_value(), true), "0%");
        assert_eq!(drive.normalized_value_to_string(1.0, true), "100%");
        assert_eq!(params.blend.default_plain_value(), 0.0);

        let dream = &params.dream_eater;
        assert_eq!(dream.default_plain_value(), 0.0);
        assert_eq!(dream.to_string(), "Off");
        assert_eq!(dream.normalized_value_to_string(dream.default_normalized_value(), true), "Off");
        assert_eq!(dream.normalized_value_to_string(0.004, true), "Off", "the knob snaps to whole percents");
        assert_eq!(dream.normalized_value_to_string(0.01, true), "1%");
        assert_eq!(dream.normalized_value_to_string(1.0, true), "100%");
        assert_eq!(dream.string_to_normalized_value("off"), Some(0.0));
        assert_eq!(dream.string_to_normalized_value("40%"), Some(0.4));

        assert!(params.restart.value() && params.restart.default_plain_value());
        assert_eq!(params.direction.value(), Direction::Forward);
        assert_eq!(params.direction.to_string(), "Forward");
        let names: Vec<String> = (0..3).map(|i| params.direction.normalized_value_to_string(i as f32 / 2.0, true)).collect();
        assert_eq!(names, ["Forward", "Backward", "Levitate"]);
        let settings = params.settings();
        assert!(settings.restart && settings.direction == Direction::Forward);
    }

    fn old_state(mode: ParamValue) -> PluginState {
        let mut params = BTreeMap::new();
        params.insert("mode".to_owned(), mode);
        params.insert("invert".to_owned(), ParamValue::Bool(true));
        params.insert("depth".to_owned(), ParamValue::F32(0.5));
        PluginState { version: "1.0.0".into(), params, fields: BTreeMap::new() }
    }

    #[test]
    fn old_states_map_mode_onto_restart() {
        for (mode, restart) in [
            (ParamValue::String("one-shot".into()), true),
            (ParamValue::String("loop".into()), false),
            (ParamValue::I32(0), true),
            (ParamValue::I32(1), false),
        ] {
            let mut state = old_state(mode.clone());
            Haunter::filter_state(&mut state);
            assert!(matches!(state.params.get("restart"), Some(ParamValue::Bool(r)) if *r == restart), "{mode:?}");
            assert!(!state.params.contains_key("mode") && !state.params.contains_key("invert"));
            assert!(matches!(state.params.get("depth"), Some(ParamValue::F32(v)) if *v == 0.5), "other parameters are kept");
        }

        let mut current = PluginState { version: "1.2.0".into(), params: BTreeMap::new(), fields: BTreeMap::new() };
        current.params.insert("restart".to_owned(), ParamValue::Bool(false));
        current.params.insert("dream-eater".to_owned(), ParamValue::F32(0.4));
        current.params.insert("direction".to_owned(), ParamValue::String("levitate".into()));
        current.params.insert("drive".to_owned(), ParamValue::F32(0.2));
        let before = format!("{:?}", current.params);
        Haunter::filter_state(&mut current);
        assert_eq!(format!("{:?}", current.params), before, "new states are left alone");
    }

    #[test]
    fn direction_normalized_values_round_trip() {
        let plugin = Haunter::default();
        let param = &plugin.params.direction;
        let ptr = param.as_ptr();
        assert_eq!(unsafe { ptr.step_count() }, Some(2));
        for (index, direction) in [Direction::Forward, Direction::Backward, Direction::Levitate].into_iter().enumerate() {
            let normalized = unsafe { ptr.preview_normalized(index as f32) };
            assert_eq!(normalized, [0.0, 0.5, 1.0][index], "{direction:?}");
            assert_eq!(param.preview_normalized(direction), normalized, "the editor sends what the wrapper expects");
            assert_eq!(param.preview_plain(normalized), direction);
            assert_eq!(unsafe { ptr.preview_plain(normalized) }, index as f32);
            // A host that hands the value back slightly off still lands on the same choice.
            for nudged in [normalized - 0.01, normalized + 0.01] {
                if (0.0..=1.0).contains(&nudged) {
                    assert_eq!(param.preview_plain(nudged), direction, "{nudged}");
                }
            }
        }
    }

    #[test]
    fn reverse_from_before_1_7_loads_as_a_direction() {
        for (reverse, direction) in [
            (Some(ParamValue::Bool(true)), "backward"),
            (Some(ParamValue::Bool(false)), "forward"),
            (None, "forward"),
        ] {
            let mut state = PluginState { version: "1.6.3".into(), params: BTreeMap::new(), fields: BTreeMap::new() };
            if let Some(reverse) = reverse.clone() {
                state.params.insert("reverse".to_owned(), reverse);
            }
            Haunter::filter_state(&mut state);
            assert!(!state.params.contains_key("reverse"));
            assert!(
                matches!(state.params.get("direction"), Some(ParamValue::String(d)) if d == direction),
                "{reverse:?} loads as {direction}: {:?}",
                state.params
            );
            // The wrapper finds the value among the enum's IDs.
            let plugin = Haunter::default();
            let ParamValue::String(id) = &state.params["direction"] else { unreachable!() };
            let ptr = plugin.params.direction.as_ptr();
            let index = Direction::ids().unwrap().iter().position(|known| known == id).expect("a Direction ID");
            assert_eq!(unsafe { ptr.preview_plain(index as f32 / 2.0) }, index as f32);
        }
    }

    /// A project saved by 1.1, with Output turned down and a sample embedded.
    fn state_from_1_1() -> PluginState {
        let mut params = BTreeMap::new();
        for (id, value) in [
            ("rate", ParamValue::String("1/8".into())),
            ("restart", ParamValue::Bool(false)),
            ("reverse", ParamValue::Bool(true)),
            ("start", ParamValue::F32(0.1)),
            ("end", ParamValue::F32(0.9)),
            ("depth", ParamValue::F32(0.5)),
            ("attack", ParamValue::F32(2.0)),
            ("release", ParamValue::F32(120.0)),
            ("hp", ParamValue::F32(HP_OFF_HZ)),
            ("lp", ParamValue::F32(LP_OFF_HZ)),
            ("sample-volume", ParamValue::F32(1.0)),
            ("blend", ParamValue::F32(0.0)),
            ("mix", ParamValue::F32(1.0)),
            ("output", ParamValue::F32(0.5)),
        ] {
            params.insert(id.to_owned(), value);
        }
        let donor = Haunter::default();
        install_sample(kick(44_100.0), &donor.params, &donor.shared);
        PluginState { version: "1.1.0".into(), params, fields: donor.params.serialize_fields() }
    }

    #[test]
    fn states_from_1_1_load_with_output_dropped_and_dream_eater_off() {
        let mut state = state_from_1_1();
        Haunter::filter_state(&mut state);
        assert!(!state.params.contains_key("output"), "Output no longer exists");
        assert!(matches!(state.params.get("depth"), Some(ParamValue::F32(v)) if *v == 0.5), "other parameters are kept");
        assert!(matches!(state.params.get("dream-eater"), Some(ParamValue::F32(v)) if *v == 0.0));
        assert!(!state.params.contains_key("reverse"), "Reverse is now Direction");
        assert!(matches!(state.params.get("direction"), Some(ParamValue::String(d)) if d == "backward"), "Reverse on is Backward");
        assert!(!state.params.contains_key("sample-volume"), "Sample Volume is folded into Blend");
        assert!(matches!(state.params.get("blend"), Some(ParamValue::F32(v)) if *v == 0.0));
        assert!(matches!(state.params.get("drive"), Some(ParamValue::F32(v)) if *v == 0.0), "Drive loads at 0");

        // The wrapper looks each saved parameter up by ID and skips any it doesn't know, so every
        // one must still exist with a matching type.
        let plugin = Haunter::default();
        let known: BTreeMap<String, ParamPtr> =
            plugin.params.param_map().into_iter().map(|(id, ptr, _)| (id, ptr)).collect();
        for (id, value) in &state.params {
            let ptr = known.get(id).unwrap_or_else(|| panic!("{id} is not a parameter any more"));
            let matching = matches!(
                (ptr, value),
                (ParamPtr::FloatParam(_), ParamValue::F32(_))
                    | (ParamPtr::BoolParam(_), ParamValue::Bool(_))
                    | (ParamPtr::EnumParam(_), ParamValue::String(_))
            );
            assert!(matching, "{id} was saved as {value:?}");
        }
        let dream = &plugin.params.dream_eater;
        assert_eq!(dream.normalized_value_to_string(dream.preview_normalized(0.0), true), "Off");

        plugin.params.deserialize_fields(&state.fields);
        assert!(plugin.params.sample.read(|s| s.is_some_and(|s| s.name == "ghost kick.wav")));
    }

    #[test]
    fn states_from_1_7_11_fold_sample_volume_into_blend_and_load_drive_at_0() {
        for (volume, blend, folded) in [(1.0, 0.6, 0.6), (0.5, 0.8, 0.4), (4.0, 0.5, 1.0), (0.25, 0.0, 0.0)] {
            let mut state = state_from_1_1();
            state.version = "1.7.11".into();
            state.params.insert("sample-volume".to_owned(), ParamValue::F32(volume));
            state.params.insert("blend".to_owned(), ParamValue::F32(blend));
            Haunter::filter_state(&mut state);
            assert!(!state.params.contains_key("sample-volume"));
            assert!(
                matches!(state.params.get("blend"), Some(ParamValue::F32(v)) if (*v - folded).abs() < 1e-6),
                "{volume} x {blend}: {:?}",
                state.params.get("blend")
            );
            assert!(matches!(state.params.get("drive"), Some(ParamValue::F32(v)) if *v == 0.0));
        }

        // A state that has Drive keeps it.
        let mut state = state_from_1_1();
        state.params.remove("sample-volume");
        state.params.insert("drive".to_owned(), ParamValue::F32(0.7));
        state.params.insert("blend".to_owned(), ParamValue::F32(0.3));
        Haunter::filter_state(&mut state);
        assert!(matches!(state.params.get("drive"), Some(ParamValue::F32(v)) if *v == 0.7));
        assert!(matches!(state.params.get("blend"), Some(ParamValue::F32(v)) if *v == 0.3));
    }

    #[test]
    fn restoring_state_replaces_the_playing_sample() {        let mut plugin = Haunter::default();
        render(&mut plugin, 48_000.0);
        assert!(plugin.engine.sample().is_none());

        let donor = Haunter::default();
        install_sample(kick(48_000.0), &donor.params, &donor.shared);
        load_state(&plugin, &save_state(&donor));
        let output = render(&mut plugin, 48_000.0);
        assert!(plugin.engine.sample().is_some());
        assert!(output.iter().any(|&v| v < 0.5), "the restored sample ducks the track");

        let empty = Haunter::default();
        load_state(&plugin, &save_state(&empty));
        render(&mut plugin, 48_000.0);
        assert!(plugin.engine.sample().is_none(), "a state without a sample clears it");
    }

    #[test]
    fn samples_are_resampled_to_the_host_rate() {
        let mut plugin = Haunter::default();
        plugin.params.sample.replace(Some(kick(44_100.0)));
        render(&mut plugin, 96_000.0);
        let prepared = plugin.engine.sample().unwrap();
        assert_eq!(prepared.sample_rate, 96_000.0);
        assert_eq!(prepared.frames(), (0.3 * 96_000.0f64).round() as usize);
        render(&mut plugin, 48_000.0);
        assert_eq!(plugin.engine.sample().unwrap().frames(), 14_400);
    }

    #[test]
    fn the_audio_thread_adopts_a_loaded_sample() {
        let mut plugin = Haunter::default();
        plugin.engine.prepare(48_000.0);
        plugin.shared.host_rate.store(48_000.0, Ordering::Release);
        install_sample(kick(48_000.0), &plugin.params, &plugin.shared);
        assert!(plugin.shared.reset_region.load(Ordering::Acquire));
        plugin.adopt_pending_sample();
        assert!(plugin.engine.sample().is_some());
        assert_eq!(plugin.engine_generation, plugin.params.sample.generation());
        assert!(plugin.shared.overview.lock().unwrap().is_some());
        assert!((plugin.shared.sample_seconds.load(Ordering::Relaxed) - 0.3).abs() < 1e-6);

        install_sample(kick(48_000.0), &plugin.params, &plugin.shared);
        plugin.adopt_pending_sample();
        assert!(plugin.shared.retired.lock().unwrap().is_some(), "the old sample is freed elsewhere");
    }

    #[test]
    fn region_readouts_use_the_sample_length() {
        let plugin = Haunter::default();
        assert_eq!(plugin.params.end.to_string(), "100%");
        plugin.shared.sample_seconds.store(0.5, Ordering::Relaxed);
        assert_eq!(plugin.params.end.to_string(), "500 ms");
        assert_eq!(plugin.params.start.string_to_normalized_value("125 ms"), Some(0.25));
        assert_eq!(plugin.params.high_pass.to_string(), "Off");
        assert_eq!(plugin.params.low_pass.to_string(), "Off");
        assert_eq!(plugin.params.rate.to_string(), "1/4");
    }
}
