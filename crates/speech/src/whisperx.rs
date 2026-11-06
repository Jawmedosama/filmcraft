//! WhisperX as a [`Transcriber`], through the local FilmCraft-AI folder ([`filmcraft_ai_bridge`]).
//!
//! The 16 kHz mono audio is written to a temporary WAV, `whisperx_worker.py` transcribes it in its
//! own Python environment (faster-whisper, then wav2vec2 word alignment), and the aligned words it
//! writes (`<out>.mots.json`) become the transcript. Speakers are labelled by our own
//! [`crate::diarize`], as for the built-in Whisper, so no Hugging Face token is needed.
//!
//! Select it with the speech model id [`MODEL_ID`] (Settings ▸ Media Analysis & Transcription).

use filmcraft_ai_bridge::{Bridge, BridgeError, Model, wav};
use filmcraft_project::{Transcript, Word};
use serde_json::Value;

use crate::{Options, ProgressFn, SAMPLE_RATE, SpeechError, Transcriber, seconds_tick};

/// Speech model id that picks WhisperX.
pub const MODEL_ID: &str = "whisperx";

/// WhisperX through the FilmCraft-AI folder.
pub struct WhisperX {
    bridge: Bridge,
}

impl WhisperX {
    /// WhisperX, when the FilmCraft-AI folder and its WhisperX environment are installed.
    pub fn find() -> Option<WhisperX> {
        filmcraft_ai_bridge::find().filter(|b| b.available(Model::WhisperX)).map(|bridge| WhisperX { bridge })
    }
}

/// Whether WhisperX is installed on this machine.
pub fn available() -> bool {
    WhisperX::find().is_some()
}

fn bridge_err(e: BridgeError) -> SpeechError {
    match e {
        BridgeError::Cancelled => SpeechError::Cancelled,
        BridgeError::NotInstalled | BridgeError::MissingEnv(_) => SpeechError::NotInstalled(MODEL_ID.into()),
        e => SpeechError::Model(e.to_string()),
    }
}

impl Transcriber for WhisperX {
    fn id(&self) -> String {
        MODEL_ID.into()
    }

    fn transcribe(&self, audio: &[f32], opts: &Options, progress: ProgressFn) -> Result<Transcript, SpeechError> {
        if !progress(0.02, "Starting WhisperX") {
            return Err(SpeechError::Cancelled);
        }
        let dir = tempdir()?;
        let input = dir.join("audio.wav");
        let srt = dir.join("transcript.srt");
        wav::write_mono16(&input, audio, SAMPLE_RATE).map_err(bridge_err)?;
        let b = &self.bridge;
        let mut args = vec!["--audio".to_string(), input.display().to_string(), "--out".into(), srt.display().to_string()];
        for (flag, key) in [("--modele", "modele"), ("--batch", "batch"), ("--compute", "compute")] {
            if let Some(v) = b.setting(Model::WhisperX, key) {
                args.extend([flag.to_string(), v]);
            }
        }
        if let Some(l) = opts.language.clone().or_else(|| b.setting(Model::WhisperX, "langue")) {
            args.extend(["--langue".to_string(), l]);
        }
        let mut frac = 0.05f32;
        let r = b.run(Model::WhisperX, &args, None, &mut |line| {
            if line.contains("alignement") {
                frac = frac.max(0.7);
            } else if !line.is_empty() {
                frac = (frac + 0.05).min(0.9);
            }
            progress(frac, if frac >= 0.7 { "Aligning words (WhisperX)" } else { "Transcribing (WhisperX)" })
        });
        let result = r.map_err(bridge_err);
        let words_path = srt.with_extension("mots.json");
        let parsed = result.and_then(|v| {
            let segments: Value =
                serde_json::from_str(&std::fs::read_to_string(&words_path)?).map_err(|e| SpeechError::Model(format!("WhisperX words: {e}")))?;
            let language = v.get("langue").and_then(Value::as_str).unwrap_or("en").to_string();
            Ok(transcript_from_segments(&segments, language))
        });
        let _ = std::fs::remove_dir_all(&dir);
        let mut t = parsed?;
        t.normalize();
        crate::vad::tighten_words(audio, &mut t.words);
        if opts.diarize && !t.words.is_empty() {
            if !progress(0.96, "Labelling speakers") {
                return Err(SpeechError::Cancelled);
            }
            let p = crate::diarize::Params { max_speakers: opts.max_speakers, ..Default::default() };
            crate::diarize::diarize(audio, &mut t, &p);
        }
        progress(1.0, "Done");
        Ok(t)
    }
}

fn tempdir() -> Result<std::path::PathBuf, SpeechError> {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let d = std::env::temp_dir().join(format!("filmcraft-whisperx-{}-{t}", std::process::id()));
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// WhisperX's aligned segments (`[{start, end, text, words: [{word, start?, end?, score?}]}]`) as
/// a transcript. Words WhisperX could not align (numbers, symbols) get the gap between their
/// aligned neighbours, inside their segment.
pub fn transcript_from_segments(segments: &Value, language: String) -> Transcript {
    let f = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0);
    let mut words = Vec::new();
    for seg in segments.as_array().map(Vec::as_slice).unwrap_or_default() {
        let (s0, s1) = (f(seg, "start").unwrap_or(0.0), f(seg, "end").unwrap_or(0.0));
        let ws: Vec<&Value> = seg.get("words").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        if ws.is_empty() {
            // no alignment for this language: one "word" per whitespace token, spread evenly
            let text = seg.get("text").and_then(Value::as_str).unwrap_or("");
            let toks: Vec<&str> = text.split_whitespace().collect();
            let step = (s1 - s0).max(0.0) / toks.len().max(1) as f64;
            for (i, t) in toks.iter().enumerate() {
                let a = s0 + step * i as f64;
                words.push(Word::new(*t, seconds_tick(a), seconds_tick(a + step)));
            }
            continue;
        }
        let mut prev_end = s0;
        for (i, w) in ws.iter().enumerate() {
            let text = w.get("word").and_then(Value::as_str).unwrap_or("").trim();
            if text.is_empty() {
                continue;
            }
            let next_start = ws[i + 1..].iter().find_map(|n| f(n, "start")).unwrap_or(s1.max(prev_end));
            let start = f(w, "start").unwrap_or(prev_end);
            let end = f(w, "end").unwrap_or(next_start).max(start);
            let mut word = Word::new(text, seconds_tick(start), seconds_tick(end));
            if let Some(score) = f(w, "score") {
                word.confidence = score.clamp(0.0, 1.0) as f32;
            }
            words.push(word);
            prev_end = end;
        }
    }
    Transcript { language, source: MODEL_ID.into(), speakers: Vec::new(), words }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn segments_become_words() {
        let segs = json!([
            {"start": 0.0, "end": 2.0, "text": "Bonjour, il est 10 h.", "words": [
                {"word": "Bonjour,", "start": 0.1, "end": 0.5, "score": 0.9},
                {"word": "il", "start": 0.6, "end": 0.7},
                {"word": "est", "start": 0.75, "end": 0.9},
                {"word": "10"},
                {"word": "h.", "start": 1.4, "end": 1.6}
            ]},
            {"start": 3.0, "end": 4.0, "text": "sans alignement ici"}
        ]);
        let t = transcript_from_segments(&segs, "fr".into());
        let texts: Vec<&str> = t.words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, ["Bonjour,", "il", "est", "10", "h.", "sans", "alignement", "ici"]);
        assert_eq!((t.words[3].start, t.words[3].end), (seconds_tick(0.9), seconds_tick(1.4)));
        assert!((t.words[0].confidence - 0.9).abs() < 1e-6);
        assert_eq!(t.words[5].start, seconds_tick(3.0));
        assert_eq!(t.language, "fr");
        assert_eq!(t.source, MODEL_ID);
    }

    #[test]
    fn garbage_is_empty() {
        assert!(transcript_from_segments(&json!({"no": "list"}), "en".into()).words.is_empty());
    }
}
