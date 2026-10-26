//! Presets: the factory ones built into the plugin, and the ones people save in their own folder.

use nih_plug::params::persist::PersistentField;
use nih_plug::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::audio_file;
use crate::sample::EmbeddedSample;

/// What the preset menu shows when no preset is selected: no sample and default settings.
pub const NO_PRESET: &str = "--";

/// Factory presets, in menu order, with the sample each one loads. They use default settings.
const FACTORY: &[(&str, &[u8])] = &[("Water", include_bytes!("../presets/Water.wav"))];

/// Bumped if the preset file format ever changes in a way older versions can't read.
const FILE_FORMAT: u32 = 1;
const FILE_EXTENSION: &str = "json";
/// Normalized difference below which a parameter still counts as the preset's value.
const TOLERANCE: f32 = 1e-4;

/// Parameter values by ID, as plain values. Enums are stored as their variant index and toggles
/// as 0 or 1.
pub type Values = BTreeMap<String, f32>;

pub fn factory_names() -> impl Iterator<Item = &'static str> {
    FACTORY.iter().map(|(name, _)| *name)
}

/// Which preset is selected.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "lowercase")]
pub enum Choice {
    #[default]
    None,
    Factory(String),
    User(String),
}

impl Choice {
    pub fn name(&self) -> &str {
        match self {
            Choice::None => NO_PRESET,
            Choice::Factory(name) | Choice::User(name) => name,
        }
    }
}

/// A preset's contents. Parameters it leaves out take their defaults.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Preset {
    pub values: Values,
    pub sample: Option<EmbeddedSample>,
}

impl Preset {
    pub fn factory(name: &str) -> Result<Preset, String> {
        let (name, wav) = FACTORY
            .iter()
            .find(|(known, _)| *known == name)
            .ok_or_else(|| format!("There is no factory preset called {name}"))?;
        let audio = audio_file::decode(wav).map_err(|err| err.to_string())?;
        let sample = EmbeddedSample::from_decoded((*name).to_owned(), audio)
            .ok_or_else(|| format!("The {name} preset has no audio"))?;
        Ok(Preset { values: Values::new(), sample: Some(sample) })
    }

    /// Every parameter's current value.
    pub fn capture(params: &impl Params, sample: Option<EmbeddedSample>) -> Preset {
        let values = params
            .param_map()
            .into_iter()
            .map(|(id, ptr, _)| (id, unsafe { ptr.unmodulated_plain_value() }))
            .collect();
        Preset { values, sample }
    }

    /// The plain value this preset gives `ptr`.
    pub fn value(&self, id: &str, ptr: &ParamPtr) -> f32 {
        // `ParamPtr::default_plain_value` returns a toggle's current value, so go through the
        // normalized default instead.
        self.values.get(id).copied().unwrap_or_else(|| unsafe { ptr.preview_plain(ptr.default_normalized_value()) })
    }

    /// Every parameter's value under this preset, with the defaults filled in.
    pub fn complete_values(&self, params: &impl Params) -> Values {
        params.param_map().into_iter().map(|(id, ptr, _)| (id.clone(), self.value(&id, &ptr))).collect()
    }
}

/// The selected preset and what it set, kept in the plugin state so a project reopens showing
/// the same preset, and whether it has been changed since.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    pub choice: Choice,
    /// What the preset set the parameters to. Missing ones are at their defaults.
    pub values: Values,
    /// [`EmbeddedSample::fingerprint`] of the preset's sample, or 0 for none.
    pub sample: u64,
}

impl Selection {
    /// Whether the parameters or the sample differ from what the preset set. With
    /// `parameters_pending`, the host hasn't had the chance to apply the preset's parameters yet,
    /// so only the sample is compared.
    pub fn is_modified(&self, params: &impl Params, sample: u64, parameters_pending: bool) -> bool {
        if sample != self.sample {
            return true;
        }
        if parameters_pending {
            return false;
        }
        params.param_map().into_iter().any(|(id, ptr, _)| unsafe {
            let target = self.values.get(&id).copied().unwrap_or_else(|| ptr.preview_plain(ptr.default_normalized_value()));
            (ptr.preview_normalized(target) - ptr.unmodulated_normalized_value()).abs() > TOLERANCE
        })
    }
}

/// The persisted [`Selection`].
#[derive(Default)]
pub struct SelectionSlot(RwLock<Selection>);

impl SelectionSlot {
    pub fn get(&self) -> Selection {
        self.0.read().unwrap().clone()
    }

    pub fn set(&self, selection: Selection) {
        *self.0.write().unwrap() = selection;
    }
}

impl<'a> PersistentField<'a, Selection> for SelectionSlot {
    fn set(&self, new_value: Selection) {
        SelectionSlot::set(self, new_value);
    }

    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Selection) -> R,
    {
        f(&self.0.read().unwrap())
    }
}

/// A user preset file: the parameters and the sample in one JSON file, the sample stored the
/// same way as in the plugin state.
#[derive(Serialize, Deserialize)]
struct PresetFile {
    haunter_preset: u32,
    name: String,
    plugin_version: String,
    params: Values,
    sample: Option<EmbeddedSample>,
}

/// The folder of user presets, one `<name>.json` per preset.
#[derive(Debug, Clone)]
pub struct UserPresets {
    dir: PathBuf,
}

impl UserPresets {
    pub fn new(dir: PathBuf) -> Self {
        UserPresets { dir }
    }

    /// `%APPDATA%\Haunter\Presets`.
    pub fn default_dir() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
            .unwrap_or_else(std::env::temp_dir);
        base.join("Haunter").join("Presets")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.{FILE_EXTENSION}"))
    }

    /// The saved presets' names, sorted. A missing folder means there are none.
    pub fn list(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if !path.is_file() || !path.extension()?.to_str()?.eq_ignore_ascii_case(FILE_EXTENSION) {
                    return None;
                }
                let name = path.file_stem()?.to_str()?.to_owned();
                check_name(&name).is_ok().then_some(name)
            })
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names
    }

    /// Saves `preset` under `name`, replacing any preset of that name, and returns the name as
    /// saved.
    pub fn save(&self, name: &str, preset: &Preset) -> Result<String, String> {
        let name = check_name(name)?.to_owned();
        let file = PresetFile {
            haunter_preset: FILE_FORMAT,
            name: name.clone(),
            plugin_version: env!("CARGO_PKG_VERSION").to_owned(),
            params: preset.values.clone(),
            sample: preset.sample.clone(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|err| format!("Could not save the preset: {err}"))?;
        let failed = |err: std::io::Error| format!("Could not save the preset: {err}");
        std::fs::create_dir_all(&self.dir).map_err(failed)?;
        // Written next to the preset and then moved over it, so a failed save never leaves a
        // half-written preset behind.
        let temporary = self.dir.join(format!("{name}.{FILE_EXTENSION}.tmp"));
        std::fs::write(&temporary, json).map_err(failed)?;
        std::fs::rename(&temporary, self.path(&name)).map_err(|err| {
            let _ = std::fs::remove_file(&temporary);
            failed(err)
        })?;
        Ok(name)
    }

    pub fn load(&self, name: &str) -> Result<Preset, String> {
        let bytes = std::fs::read(self.path(name)).map_err(|err| format!("Could not open the preset {name}: {err}"))?;
        let file: PresetFile =
            serde_json::from_slice(&bytes).map_err(|err| format!("The preset {name} is damaged: {err}"))?;
        if file.haunter_preset > FILE_FORMAT {
            return Err(format!("The preset {name} needs a newer version of Haunter"));
        }
        let values = file.params.into_iter().filter(|(_, value)| value.is_finite()).collect();
        Ok(Preset { values, sample: file.sample })
    }
}

/// Trims `name` and checks it can be a preset name and a file name.
pub fn check_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Type a name for the preset".to_owned());
    }
    if name.chars().count() > 64 {
        return Err("Preset names can be up to 64 characters".to_owned());
    }
    if name.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c)) || name.ends_with('.') {
        return Err(r#"Preset names can't contain < > : " / \ | ? * or end with a dot"#.to_owned());
    }
    let stem = name.split('.').next().unwrap_or(name).trim_end().to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(format!("Windows doesn't allow {name} as a file name"));
    }
    if name == NO_PRESET || factory_names().any(|factory| factory.eq_ignore_ascii_case(name)) {
        return Err(format!("{name} is a factory preset; choose another name"));
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_must_work_as_file_names() {
        assert_eq!(check_name("  Big Room  "), Ok("Big Room"));
        for bad in ["", "   ", "a/b", "a\\b", "what?", "con", "COM1", "lpt9.txt", "dots.", "--", "water", "Water"] {
            assert!(check_name(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(check_name(&"x".repeat(64)).is_ok());
        assert!(check_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn the_water_preset_decodes_its_embedded_sample() {
        let water = Preset::factory("Water").unwrap();
        let sample = water.sample.unwrap();
        assert_eq!(sample.name, "Water");
        assert_eq!(sample.sample_rate, 44_100.0);
        assert_eq!(sample.channels.len(), 2);
        assert_eq!(sample.frames(), 184_275);
        assert!(water.values.is_empty(), "default settings");
        assert!(Preset::factory("Fire").is_err());
    }

    #[test]
    fn choices_serialize_with_their_kind() {
        let user = serde_json::to_string(&Choice::User("Mine".into())).unwrap();
        assert_eq!(user, r#"{"kind":"user","name":"Mine"}"#);
        assert_eq!(serde_json::to_string(&Choice::None).unwrap(), r#"{"kind":"none"}"#);
        assert_eq!(serde_json::from_str::<Choice>(r#"{"kind":"factory","name":"Water"}"#).unwrap(), Choice::Factory("Water".into()));
    }
}
