//! AI Studio panel: the local AI models of the FilmCraft-AI folder, in one place.
//!
//! - **Generate a shot** (WanGP): a prompt and a length; the shot is generated in the background
//!   (`ai.generateShot`), then imported and placed on a new video track at the playhead.
//! - **Transcribe** (WhisperX): runs `transcript.generate` with the WhisperX model, so the Text
//!   panel's transcript, text-based editing and captions work from it.
//! - **Voice-over** (Chatterbox): opens Text to Speech, where the Chatterbox voices are listed.
//!
//! Automation ids: `ai.prompt`, `ai.seconds`, `ai.generate`, `ai.cancel`, `ai.transcribe`,
//! `ai.voiceover`.

use egui::Rect;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::FilmcraftApp;

/// What the panel is doing (UI state, serde so agents can read it).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AiDraft {
    pub prompt: String,
    pub seconds: f64,
    /// The running generation job.
    pub job: Option<u64>,
    /// Last outcome shown under the button.
    pub message: String,
    /// `ai.status`, refreshed every few seconds (it reads FilmCraft-AI's config from disk).
    #[serde(skip)]
    pub status: Option<(f64, serde_json::Value)>,
}

impl Default for AiDraft {
    fn default() -> Self {
        AiDraft { prompt: String::new(), seconds: 3.0, job: None, message: String::new(), status: None }
    }
}

/// Place a finished shot; called every frame (from `frame.rs`).
pub fn poll(app: &mut FilmcraftApp) {
    use std::sync::atomic::Ordering;
    let Some(id) = app.ui.ai.job else { return };
    let outcome = match app.session.jobs.iter().find(|j| j.id == id) {
        None => Some(Err("the generation job disappeared".to_string())),
        Some(j) if j.progress.finished.load(Ordering::Relaxed) => Some(match &*j.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
            Some(Ok(r)) => Ok(r.path.clone()),
            Some(Err(e)) => Err(e.clone()),
            None => Err("the generation stopped".into()),
        }),
        Some(_) => None,
    };
    let Some(outcome) = outcome else { return };
    app.ui.ai.job = None;
    app.ui.ai.message = match outcome.and_then(|path| place(app, &path).map(|()| path)) {
        Ok(path) => {
            let name = std::path::Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(path);
            app.ui.status = format!("AI shot placed: {name}");
            format!("Placed {name} on a new video track")
        }
        Err(e) => {
            app.ui.status = format!("AI Studio: {e}");
            e
        }
    };
}

fn place(app: &mut FilmcraftApp, path: &str) -> Result<(), String> {
    let r = app.session.execute("file.import", json!({"paths": [path]})).map_err(|e| e.to_string())?;
    let item = r["items"].as_array().and_then(|a| a.first()).and_then(serde_json::Value::as_u64).ok_or_else(|| format!("could not import {path}"))?;
    if app.session.active_sequence().is_none() {
        return Ok(()); // imported into the project; nothing to place it in
    }
    app.session.execute("timeline.place", json!({"item": item, "track": "new"})).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn show(app: &mut FilmcraftApp, ui: &mut egui::Ui, rect: Rect) {
    let t = app.tokens;
    ui.painter().rect_filled(rect, 0.0, t.panel_bg);
    let now = ui.input(|i| i.time);
    let status = match &app.ui.ai.status {
        Some((at, v)) if now - at < 3.0 => v.clone(),
        _ => {
            let v = app.session.execute("ai.status", json!({})).unwrap_or_default();
            app.ui.ai.status = Some((now, v.clone()));
            v
        }
    };
    let has = |k: &str| status[k].as_bool().unwrap_or(false);
    let running = app
        .ui
        .ai
        .job
        .and_then(|id| app.session.jobs.iter().find(|j| j.id == id))
        .map(|j| (j.progress.fraction(), j.progress.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()));
    let has_seq = app.session.active_sequence().is_some();
    let mut generate = false;
    let mut cancel = false;
    let mut transcribe = false;
    let mut voiceover = false;
    let body = rect.shrink2(egui::vec2(14.0, 8.0));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(body).id_salt("ai-studio-body"));
    let auto = &mut app.auto;
    let d = &mut app.ui.ai;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(&mut child, |ui| {
        let w = ui.available_width() - 6.0;
        if !has("installed") {
            ui.label(egui::RichText::new("FilmCraft-AI is not installed").color(t.text).strong());
            ui.label(
                egui::RichText::new("Put the FilmCraft-AI folder on your Desktop (or set FILMCRAFT_AI_DIR) and run its installer.bat.")
                    .color(t.text_dim)
                    .size(11.5),
            );
            return;
        }
        let row = |ui: &mut egui::Ui, name: &str, ok: bool| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(if ok { "●" } else { "○" }).color(if ok { t.accent } else { t.text_faint }));
                ui.label(egui::RichText::new(name).color(if ok { t.text } else { t.text_faint }).size(12.0));
                if !ok {
                    ui.label(egui::RichText::new("not set up").color(t.text_faint).size(11.0));
                }
            });
        };
        row(ui, "WhisperX: transcription and captions", has("whisperx"));
        row(ui, "Chatterbox: voice-over and voice cloning", has("chatterbox"));
        row(ui, "WanGP: shot generation", has("wangp"));
        ui.add_space(10.0);

        ui.label(egui::RichText::new("Generate a shot").color(t.text).size(12.5).strong());
        let te = egui::TextEdit::multiline(&mut d.prompt)
            .hint_text("Describe the shot: subject, action, camera, light")
            .desired_width(w)
            .desired_rows(4)
            .char_limit(4000);
        let r = crate::widgets::accessible_name(ui.add_enabled(running.is_none(), te), "Prompt");
        auto.add("ai.prompt", r.rect.intersect(ui.clip_rect()), "Prompt");
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Length").color(t.text_dim).size(11.5));
            let r = ui.add_enabled(running.is_none(), egui::DragValue::new(&mut d.seconds).range(1.0..=20.0).speed(0.1).suffix(" s"));
            auto.add("ai.seconds", r.rect.intersect(ui.clip_rect()), "Length");
            if let Some(m) = status["wangpModel"].as_str() {
                ui.label(egui::RichText::new(m).color(t.text_faint).size(11.0));
            }
        });
        match &running {
            Some((frac, st)) => {
                ui.label(egui::RichText::new(if st.is_empty() { "Generating…" } else { st.as_str() }).color(t.text).size(11.5));
                ui.add(egui::ProgressBar::new(*frac).desired_width(w));
                let b = ui.button("Cancel");
                auto.add("ai.cancel", b.rect.intersect(ui.clip_rect()), "Cancel");
                cancel = b.clicked();
            }
            None => {
                let ok = has("wangp") && !d.prompt.trim().is_empty();
                let b = ui.add_enabled(ok, egui::Button::new("Generate and place at the playhead"));
                auto.add("ai.generate", b.rect.intersect(ui.clip_rect()), "Generate");
                generate = b.clicked() && ok;
            }
        }
        if !d.message.is_empty() {
            ui.label(egui::RichText::new(&d.message).color(t.text_dim).size(11.0));
        }
        ui.label(egui::RichText::new(status["wangpNotice"].as_str().unwrap_or("")).color(t.text_faint).size(10.5));
        ui.add_space(12.0);

        ui.label(egui::RichText::new("Speech").color(t.text).size(12.5).strong());
        ui.horizontal_wrapped(|ui| {
            let b = ui.add_enabled(has("whisperx") && has_seq, egui::Button::new("Transcribe the sequence (WhisperX)"));
            auto.add("ai.transcribe", b.rect.intersect(ui.clip_rect()), "Transcribe");
            transcribe = b.clicked();
            let b = ui.add_enabled(has("chatterbox"), egui::Button::new("Voice-over (Chatterbox)…"));
            auto.add("ai.voiceover", b.rect.intersect(ui.clip_rect()), "Voice-over");
            voiceover = b.clicked();
        });
    });

    if generate {
        let p = json!({"prompt": app.ui.ai.prompt.trim(), "seconds": app.ui.ai.seconds});
        match app.session.execute("ai.generateShot", p) {
            Ok(r) => {
                app.ui.ai.job = r["job"].as_u64();
                app.ui.ai.message.clear();
                app.ui.status = "Generating a shot with WanGP…".into();
            }
            Err(e) => app.ui.ai.message = e.to_string(),
        }
    }
    if cancel && let Some(id) = app.ui.ai.job {
        let _ = app.session.execute("jobs.cancel", json!({"job": id}));
    }
    if transcribe {
        app.ui.status = match app.session.execute("transcript.generate", json!({"model": filmcraft_speech_model()})) {
            Ok(_) => {
                app.show_panel(crate::dock::PanelKind::Text);
                "Transcribed with WhisperX: see the Text panel ▸ Transcript".into()
            }
            Err(e) => format!("Transcription: {e}"),
        };
    }
    if voiceover {
        if !app.ui.tts.voice.starts_with("chatterbox-") {
            app.ui.tts.language = "fr-FR".into();
            app.ui.tts.voice = "chatterbox-fr".into();
        }
        app.show_panel(crate::dock::PanelKind::TextToSpeech);
    }
}

fn filmcraft_speech_model() -> &'static str {
    "whisperx"
}
