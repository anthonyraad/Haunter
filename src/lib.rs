use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use nih_plug::wrapper::state::{ParamValue, PluginState};
use nih_plug_egui::EguiState;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub mod audio_file;
pub mod dsp;
mod editor;
pub mod sample;

use dsp::{Engine, FrameGains, Settings, SyncRate, TransportInfo, HP_OFF_HZ, LP_OFF_HZ};
use sample::{EmbeddedSample, Overview, PreparedSample};

/// The persisted sample. Every replacement, including one from a loaded project or preset, bumps
/// the generation so the plugin knows to rebuild its playback copy.
#[derive(Default)]
pub struct SampleSlot {
    sample: RwLock<Option<EmbeddedSample>>,
    generation: AtomicU64,
}

impl SampleSlot {
    /// Stores a new sample and returns its generation.
    pub fn replace(&self, sample: Option<EmbeddedSample>) -> u64 {
        *self.sample.write().unwrap() = sample;
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
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

struct PendingSample {
    generation: u64,
    prepared: Arc<PreparedSample>,
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
}

impl Shared {
    fn new() -> Self {
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

/// Decodes and installs a WAV or AIFF on a background thread.
pub fn load_file(path: PathBuf, params: Arc<GhostpumpParams>, shared: Arc<Shared>) {
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

pub fn install_sample(sample: EmbeddedSample, params: &GhostpumpParams, shared: &Shared) {
    let host_rate = shared.host_rate.load(Ordering::Acquire) as f64;
    let prepared = Arc::new(PreparedSample::new(&sample, host_rate));
    shared.show_sample(Some(&sample));
    let generation = params.sample.replace(Some(sample));
    shared.retired.lock().unwrap().take();
    *shared.pending.lock().unwrap() = Some(PendingSample { generation, prepared });
    shared.reset_region.store(true, Ordering::Release);
}

#[derive(Params)]
pub struct GhostpumpParams {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    #[persist = "sample"]
    pub sample: SampleSlot,

    #[id = "rate"]
    pub rate: EnumParam<SyncRate>,
    #[id = "restart"]
    pub restart: BoolParam,
    #[id = "reverse"]
    pub reverse: BoolParam,
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

    #[id = "sample-volume"]
    pub sample_volume: FloatParam,
    #[id = "blend"]
    pub blend: FloatParam,

    #[id = "mix"]
    pub mix: FloatParam,
    #[id = "output"]
    pub output: FloatParam,
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

fn gain_param(name: &str, default_db: f32, min_db: f32, max_db: f32) -> FloatParam {
    FloatParam::new(
        name,
        util::db_to_gain(default_db),
        FloatRange::Skewed {
            min: util::db_to_gain(min_db),
            max: util::db_to_gain(max_db),
            factor: FloatRange::gain_skew_factor(min_db, max_db),
        },
    )
    .with_smoother(SmoothingStyle::Logarithmic(20.0))
    .with_unit(" dB")
    .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
    .with_string_to_value(formatters::s2v_f32_gain_to_db())
}

impl GhostpumpParams {
    fn new(sample_seconds: Arc<AtomicF32>) -> Self {
        GhostpumpParams {
            editor_state: EguiState::from_size(editor::WIDTH, editor::HEIGHT),
            sample: SampleSlot::default(),
            rate: EnumParam::new("Rate", SyncRate::Quarter),
            restart: BoolParam::new("Restart", true),
            reverse: BoolParam::new("Reverse", false),
            start: region_param("Start", 0.0, sample_seconds.clone()),
            end: region_param("End", 1.0, sample_seconds),
            depth: percent_param("Depth", 0.75),
            attack: time_param("Attack", 1.0, 0.1, 100.0),
            release: time_param("Release", 80.0, 5.0, 1000.0),
            high_pass: cutoff_param("HP", HP_OFF_HZ, HP_OFF_HZ, 2_000.0, HP_OFF_HZ),
            low_pass: cutoff_param("LP", LP_OFF_HZ, 50.0, LP_OFF_HZ, LP_OFF_HZ),
            sample_volume: gain_param("Sample Volume", 0.0, -48.0, 12.0),
            blend: percent_param("Sample Blend", 0.0),
            mix: percent_param("Mix", 1.0),
            output: gain_param("Output", 0.0, -24.0, 12.0),
        }
    }

    fn settings(&self) -> Settings {
        Settings {
            rate: self.rate.value(),
            restart: self.restart.value(),
            reverse: self.reverse.value(),
            start: self.start.value(),
            end: self.end.value(),
            attack_ms: self.attack.value(),
            release_ms: self.release.value(),
            high_pass_hz: self.high_pass.value(),
            low_pass_hz: self.low_pass.value(),
        }
    }
}

/// Brings states saved before Restart and Reverse existed up to date. One-shot mode becomes
/// Restart on and Loop mode becomes Restart off. Invert has no replacement and is dropped.
fn migrate_state(state: &mut PluginState) {
    if let Some(mode) = state.params.remove("mode") {
        let looped = matches!(&mode, ParamValue::String(id) if id == "loop") || matches!(mode, ParamValue::I32(1));
        state.params.entry("restart".to_owned()).or_insert(ParamValue::Bool(!looped));
    }
    state.params.remove("invert");
}

pub struct Ghostpump {
    params: Arc<GhostpumpParams>,
    shared: Arc<Shared>,
    engine: Engine,
    /// The sample generation the engine is playing.
    engine_generation: u64,
}

impl Default for Ghostpump {
    fn default() -> Self {
        let shared = Arc::new(Shared::new());
        let params = Arc::new(GhostpumpParams::new(shared.sample_seconds.clone()));
        #[cfg(feature = "standalone")]
        if let Some(path) = std::env::var_os("GHOSTPUMP_PREVIEW_SAMPLE") {
            load_file(path.into(), params.clone(), shared.clone());
        }
        Ghostpump {
            params,
            shared,
            engine: Engine::new(48_000.0),
            engine_generation: 0,
        }
    }
}

impl Ghostpump {
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
        if next.prepared.sample_rate != self.engine.sample_rate() {
            return;
        }
        let old = self.engine.set_sample(Some(next.prepared));
        self.engine_generation = next.generation;
        if let (Some(old), Ok(mut retired)) = (old, self.shared.retired.try_lock()) {
            if retired.is_none() {
                *retired = Some(old);
            }
        }
    }
}

impl Plugin for Ghostpump {
    const NAME: &'static str = "Ghostpump";
    const VENDOR: &'static str = "Ghostpump";
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
        self.adopt_pending_sample();
        self.engine.settings = self.params.settings();

        let transport = context.transport();
        let sample_rate = self.engine.sample_rate();
        let info = TransportInfo {
            playing: transport.playing,
            beat: transport.pos_beats(),
            beats_per_frame: transport.tempo.map(|tempo| tempo / 60.0 / sample_rate),
        };

        let params = &self.params;
        let report = self.engine.process(buffer.as_slice(), &info, || {
            let volume = params.sample_volume.smoothed.next();
            let blend = params.blend.smoothed.next();
            FrameGains {
                depth: params.depth.smoothed.next(),
                mix: params.mix.smoothed.next(),
                sample_level: volume * blend,
                output: params.output.smoothed.next(),
            }
        });

        if params.editor_state.is_open() {
            let reduction = -util::gain_to_db(report.min_track_gain.max(1e-6));
            let meter = &self.shared.gain_reduction_db;
            if reduction > meter.load(Ordering::Relaxed) {
                meter.store(reduction, Ordering::Relaxed);
            }
            self.shared.playhead.store(report.playhead.unwrap_or(-1.0), Ordering::Relaxed);
        }

        ProcessStatus::Normal
    }
}

impl Vst3Plugin for Ghostpump {
    const VST3_CLASS_ID: [u8; 16] = *b"GhostpumpDuckFx1";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Fx, Vst3SubCategory::Dynamics];
}

nih_export_vst3!(Ghostpump);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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
    fn render(plugin: &mut Ghostpump, host_rate: f64) -> Vec<f32> {
        plugin.engine.prepare(host_rate);
        plugin.sync_sample(host_rate);
        plugin.engine.finish_analysis();
        let frames = host_rate as usize;
        let mut left = vec![1.0f32; frames];
        let mut right = vec![1.0f32; frames];
        let gains = FrameGains { depth: 0.8, mix: 1.0, sample_level: 0.5, output: 1.0 };
        plugin.engine.process(
            &mut [&mut left[..], &mut right[..]],
            &TransportInfo::playing_at(0.0, 120.0, host_rate),
            || gains,
        );
        left
    }

    /// The non-parameter part of the plugin state, serialized the way nih-plug's wrapper does it:
    /// each persisted field as a JSON string, inside the JSON state object.
    fn save_state(plugin: &Ghostpump) -> String {
        serde_json::to_string(&plugin.params.serialize_fields()).unwrap()
    }

    fn load_state(plugin: &Ghostpump, state: &str) {
        let fields: BTreeMap<String, String> = serde_json::from_str(state).unwrap();
        plugin.params.deserialize_fields(&fields);
    }

    #[test]
    fn state_embeds_the_sample_and_restores_it() {
        let mut original = Ghostpump::default();
        install_sample(kick(44_100.0), &original.params, &original.shared);
        let state = save_state(&original);
        assert!(state.contains("ghost kick.wav"));

        let mut restored = Ghostpump::default();
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
    fn parameter_ids_are_stable() {
        let plugin = Ghostpump::default();
        let ids: Vec<String> = plugin.params.param_map().into_iter().map(|(id, _, _)| id).collect();
        for id in [
            "rate", "restart", "reverse", "start", "end", "depth", "attack", "release", "hp", "lp",
            "sample-volume", "blend", "mix", "output",
        ] {
            assert!(ids.iter().any(|known| known == id), "missing parameter {id}");
        }
        assert!(!ids.iter().any(|id| id == "mode" || id == "invert"));
        let fields = plugin.params.serialize_fields();
        assert_eq!(fields.get("sample").map(String::as_str), Some("null"));
        assert!(fields.contains_key("editor-state"));
    }

    #[test]
    fn defaults() {
        let params = &Ghostpump::default().params;
        for gain in [&params.sample_volume, &params.output] {
            assert_eq!(gain.default_plain_value(), 1.0);
            assert_eq!(gain.to_string(), "0.0 dB");
            // What the host shows for the default it was given.
            assert_eq!(gain.normalized_value_to_string(gain.default_normalized_value(), true), "0.0 dB");
        }
        assert!(params.restart.value() && params.restart.default_plain_value());
        assert!(!params.reverse.value());
        let settings = params.settings();
        assert!(settings.restart && !settings.reverse);
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
            Ghostpump::filter_state(&mut state);
            assert!(matches!(state.params.get("restart"), Some(ParamValue::Bool(r)) if *r == restart), "{mode:?}");
            assert!(!state.params.contains_key("mode") && !state.params.contains_key("invert"));
            assert!(matches!(state.params.get("depth"), Some(ParamValue::F32(v)) if *v == 0.5), "other parameters are kept");
        }

        let mut current = PluginState { version: "1.1.0".into(), params: BTreeMap::new(), fields: BTreeMap::new() };
        current.params.insert("restart".to_owned(), ParamValue::Bool(false));
        let before = format!("{:?}", current.params);
        Ghostpump::filter_state(&mut current);
        assert_eq!(format!("{:?}", current.params), before, "new states are left alone");
    }

    #[test]
    fn restoring_state_replaces_the_playing_sample() {
        let mut plugin = Ghostpump::default();
        render(&mut plugin, 48_000.0);
        assert!(plugin.engine.sample().is_none());

        let donor = Ghostpump::default();
        install_sample(kick(48_000.0), &donor.params, &donor.shared);
        load_state(&plugin, &save_state(&donor));
        let output = render(&mut plugin, 48_000.0);
        assert!(plugin.engine.sample().is_some());
        assert!(output.iter().any(|&v| v < 0.5), "the restored sample ducks the track");

        let empty = Ghostpump::default();
        load_state(&plugin, &save_state(&empty));
        render(&mut plugin, 48_000.0);
        assert!(plugin.engine.sample().is_none(), "a state without a sample clears it");
    }

    #[test]
    fn samples_are_resampled_to_the_host_rate() {
        let mut plugin = Ghostpump::default();
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
        let mut plugin = Ghostpump::default();
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
        let plugin = Ghostpump::default();
        assert_eq!(plugin.params.end.to_string(), "100%");
        plugin.shared.sample_seconds.store(0.5, Ordering::Relaxed);
        assert_eq!(plugin.params.end.to_string(), "500 ms");
        assert_eq!(plugin.params.start.string_to_normalized_value("125 ms"), Some(0.25));
        assert_eq!(plugin.params.high_pass.to_string(), "Off");
        assert_eq!(plugin.params.low_pass.to_string(), "Off");
        assert_eq!(plugin.params.rate.to_string(), "1/4");
    }
}
