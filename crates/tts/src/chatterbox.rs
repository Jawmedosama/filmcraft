//! Chatterbox voices (Resemble AI, MIT), run by the local FilmCraft-AI folder
//! ([`filmcraft_ai_bridge`]) in its own Python environment, on the GPU when there is one.
//!
//! Each language has a stock voice (`chatterbox-<lang>`) and a cloned one
//! (`chatterbox-clone-<lang>`) that speaks with the voice of the reference WAV set as
//! `chatterbox.voix_reference` in FilmCraft-AI's `config.json` (5 to 10 seconds of clean speech).
//! Pause markers work as for every voice: each spoken run is synthesized on its own and silence is
//! inserted between them. Pace is applied by resampling (which also shifts the pitch a little); the pitch control is ignored.

use filmcraft_ai_bridge::{BridgeError, Model, wav};

use crate::script::{self, Segment};
use crate::{Audio, Gender, Params, SAMPLE_RATE, TtsError, Voice, VoiceInfo, check_text};

/// The engine name these voices carry in [`VoiceInfo::engine`].
pub const ENGINE: &str = "chatterbox";

const fn v(id: &'static str, name: &'static str, language: &'static str, description: &'static str) -> VoiceInfo {
    VoiceInfo { id, name, language, gender: Gender::Female, engine: ENGINE, description, license: "MIT (Chatterbox, Resemble AI)", installed: false }
}

/// Every Chatterbox voice.
pub const VOICES: &[VoiceInfo] = &[
    v("chatterbox-fr", "Chatterbox", "fr-FR", "Natural French voice (Chatterbox Multilingual, FilmCraft-AI)"),
    v("chatterbox-clone-fr", "Chatterbox: my cloned voice", "fr-FR", "Speaks with the voice of FilmCraft-AI's reference recording"),
    v("chatterbox-en", "Chatterbox", "en-US", "Natural English voice (Chatterbox, FilmCraft-AI)"),
    v("chatterbox-clone-en", "Chatterbox: my cloned voice", "en-US", "Speaks with the voice of FilmCraft-AI's reference recording"),
    v("chatterbox-es", "Chatterbox", "es-ES", "Natural Spanish voice (Chatterbox Multilingual, FilmCraft-AI)"),
    v("chatterbox-clone-es", "Chatterbox: my cloned voice", "es-ES", "Speaks with the voice of FilmCraft-AI's reference recording"),
    v("chatterbox-de", "Chatterbox", "de-DE", "Natural German voice (Chatterbox Multilingual, FilmCraft-AI)"),
    v("chatterbox-clone-de", "Chatterbox: my cloned voice", "de-DE", "Speaks with the voice of FilmCraft-AI's reference recording"),
    v("chatterbox-it", "Chatterbox", "it-IT", "Natural Italian voice (Chatterbox Multilingual, FilmCraft-AI)"),
    v("chatterbox-clone-it", "Chatterbox: my cloned voice", "it-IT", "Speaks with the voice of FilmCraft-AI's reference recording"),
];

/// The voices with `installed` set from this machine's FilmCraft-AI folder.
pub fn voices() -> Vec<VoiceInfo> {
    let bridge = filmcraft_ai_bridge::find().filter(|b| b.available(Model::Chatterbox));
    let has_ref = bridge.as_ref().and_then(|b| b.setting(Model::Chatterbox, "voix_reference")).is_some_and(|p| std::path::Path::new(&p).is_file());
    VOICES.iter().map(|v| VoiceInfo { installed: bridge.is_some() && (!is_clone(v.id) || has_ref), ..*v }).collect()
}

fn is_clone(id: &str) -> bool {
    id.starts_with("chatterbox-clone-")
}

/// A Chatterbox voice.
pub struct ChatterboxVoice {
    info: VoiceInfo,
}

/// The Chatterbox voice `id`, if it is one.
pub fn find(id: &str) -> Option<ChatterboxVoice> {
    VOICES.iter().find(|v| v.id == id).map(|v| ChatterboxVoice { info: *v })
}

fn err(e: BridgeError) -> TtsError {
    match e {
        BridgeError::NotInstalled | BridgeError::MissingEnv(_) => {
            TtsError::Model("Chatterbox is not installed: run installer.bat in the FilmCraft-AI folder".into())
        }
        e => TtsError::Model(e.to_string()),
    }
}

impl Voice for ChatterboxVoice {
    fn info(&self) -> VoiceInfo {
        self.info
    }

    fn synthesize(&self, text: &str, params: &Params) -> Result<Audio, TtsError> {
        check_text(text)?;
        params.check()?;
        let bridge = filmcraft_ai_bridge::find().ok_or_else(|| err(BridgeError::NotInstalled))?;
        let lang = self.info.language.split('-').next().unwrap_or("fr").to_string();
        let reference = if is_clone(self.info.id) {
            Some(
                bridge
                    .setting(Model::Chatterbox, "voix_reference")
                    .ok_or_else(|| TtsError::Model("no reference recording: set chatterbox.voix_reference in FilmCraft-AI's config.json".into()))?,
            )
        } else {
            None
        };
        let mut out = Vec::new();
        for seg in script::parse(text) {
            match seg {
                Segment::Pause { millis } => out.extend(std::iter::repeat_n(0.0, (SAMPLE_RATE as usize * millis as usize) / 1000)),
                Segment::Speech(s) if s.trim().is_empty() => {}
                Segment::Speech(s) => {
                    let path = bridge.output_path("narration", "wav").map_err(err)?;
                    let mut args =
                        vec!["--texte".to_string(), s.trim().to_string(), "--out".into(), path.display().to_string(), "--langue".into(), lang.clone()];
                    if let Some(r) = &reference {
                        args.extend(["--voix".to_string(), r.clone()]);
                    }
                    bridge.run(Model::Chatterbox, &args, None, &mut |_| true).map_err(err)?;
                    let m = wav::read_mono(&path).map_err(err)?;
                    let _ = std::fs::remove_file(&path);
                    // pace: play the speech faster or slower by resampling
                    let rate = (f64::from(SAMPLE_RATE) / params.pace).round() as u32;
                    out.extend(wav::resample(&m.samples, m.sample_rate, rate.max(1)));
                }
            }
        }
        if out.is_empty() {
            return Err(TtsError::Empty);
        }
        Ok(Audio { samples: out, sample_rate: SAMPLE_RATE })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue() {
        assert!(find("chatterbox-fr").is_some());
        assert!(find("kokoro-heart").is_none());
        assert!(VOICES.iter().all(|v| v.engine == ENGINE && crate::LANGUAGES.iter().any(|(l, _)| *l == v.language)));
        // no FilmCraft-AI folder on a test machine: nothing is installed
        if filmcraft_ai_bridge::find().is_none() {
            assert!(voices().iter().all(|v| !v.installed));
            assert!(matches!(find("chatterbox-fr").unwrap().synthesize("Bonjour", &Params::default()), Err(TtsError::Model(_))));
        }
    }
}
