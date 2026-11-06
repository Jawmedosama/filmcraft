//! Local generative AI (`ai.*`): the models of the FilmCraft-AI folder ([`filmcraft_ai_bridge`]).
//!
//! - `ai.status`: which models are set up on this machine.
//! - `ai.generateShot`: generate a video shot (or still) from a prompt with WanGP, as a background
//!   job. When the job finishes, its `result.path` is the generated file; the AI Studio panel then
//!   imports it and places it on a new video track at the playhead (`file.import`,
//!   `timeline.place`), and agents can do the same.
//!
//! WhisperX and Chatterbox need no commands of their own: they are a speech model of
//! `transcript.generate` (`model: "whisperx"`) and voices of Text to Speech (`chatterbox-*`).
//!
//! WanGP is under the WanGP Community License: free to use, but it may not be embedded in a paid
//! product or service without a commercial licence, and products that use it must say so.

use std::sync::PoisonError;

use serde_json::{Value, json};

use filmcraft_ai_bridge::{Bridge, Model};

use crate::commands::{CommandSpec, always, bad, str_p};
use crate::{EngineError, Result, Session};

/// Licence notice the UI shows next to generation.
pub const WANGP_NOTICE: &str = "Video generation by WanGP (WanGP Community License: no use inside a paid product without a commercial licence)";

fn bridge() -> Option<Bridge> {
    filmcraft_ai_bridge::find()
}

fn status(_: &mut Session, _: &Value) -> Result<Value> {
    let b = bridge();
    let has = |m: Model| b.as_ref().is_some_and(|b| b.available(m));
    Ok(json!({
        "installed": b.is_some(),
        "dir": b.as_ref().map(|b| b.root.to_string_lossy().to_string()),
        "whisperx": has(Model::WhisperX),
        "chatterbox": has(Model::Chatterbox),
        "wangp": has(Model::WanGP),
        "wangpModel": b.as_ref().and_then(|b| b.setting(Model::WanGP, "modele")),
        "wangpNotice": WANGP_NOTICE,
    }))
}

fn can_generate(_: &Session) -> std::result::Result<(), String> {
    match bridge() {
        Some(b) if b.available(Model::WanGP) => Ok(()),
        Some(_) => Err("WanGP is not set up: run installer.bat wangp in the FilmCraft-AI folder".into()),
        None => Err("FilmCraft-AI is not installed".into()),
    }
}

/// Settings of one generation, from the call and FilmCraft-AI's `config.json`.
fn shot_args(b: &Bridge, p: &Value) -> std::result::Result<(Vec<String>, Option<String>), String> {
    let cmd = "ai.generateShot";
    let prompt = str_p(p, "prompt").map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| format!("{cmd}: need a `prompt`"))?;
    if prompt.len() > 4000 {
        return Err(format!("{cmd}: the prompt is longer than 4000 bytes"));
    }
    let dir = b.setting(Model::WanGP, "dossier").ok_or_else(|| format!("{cmd}: set wangp.dossier (the WanGP folder) in FilmCraft-AI's config.json"))?;
    let pick = |key: &str, cfg: &str, default: &str| {
        str_p(p, key).map(str::to_string).filter(|s| !s.is_empty()).or_else(|| b.setting(Model::WanGP, cfg)).unwrap_or_else(|| default.to_string())
    };
    let duration = match p.get("seconds").and_then(Value::as_f64) {
        Some(s) if s > 0.0 && s <= 60.0 => format!("{s}s"),
        Some(_) => return Err(format!("{cmd}: `seconds` must be between 0 and 60")),
        None => pick("duration", "duree", "5s"),
    };
    let mut args = vec![
        "--wangp".to_string(),
        dir.clone(),
        "--prompt".into(),
        prompt.to_string(),
        "--modele".into(),
        pick("model", "modele", "t2v_1.3B"),
        "--duree".into(),
        duration,
        "--profil".into(),
        pick("profile", "profil", "4"),
    ];
    let res = pick("resolution", "resolution", "");
    if !res.is_empty() {
        args.extend(["--resolution".to_string(), res]);
    }
    if let Some(img) = str_p(p, "startImage").filter(|s| !s.is_empty()) {
        args.extend(["--image-debut".to_string(), img.to_string()]);
    }
    Ok((args, Some(dir)))
}

fn generate_shot(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "ai.generateShot";
    let b = bridge().ok_or_else(|| bad(cmd, "FilmCraft-AI is not installed"))?;
    let (args, cwd) = shot_args(&b, p).map_err(EngineError::Other)?;
    let id = s.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let job = crate::Job { id, label: "Generating a shot (WanGP)".into(), progress: Default::default(), result: Default::default() };
    let (prog, res) = (job.progress.clone(), job.result.clone());
    let wait = p.get("wait").and_then(Value::as_bool).unwrap_or(false);
    let run = crate::export_tools::guard_job(prog.clone(), res.clone(), move || {
        use std::sync::atomic::Ordering;
        let t0 = web_time::Instant::now();
        let set = |msg: &str| *prog.status.lock().unwrap_or_else(PoisonError::into_inner) = msg.to_string();
        set("Starting WanGP…");
        let r = b.run(Model::WanGP, &args, cwd.as_deref().map(std::path::Path::new), &mut |line| {
            if let Some(rest) = line.strip_prefix("[wangp]") {
                set(rest.trim());
                // "<phase> <step>/<total>": show the steps as progress
                if let Some((d, t)) = rest.split_whitespace().last().and_then(|x| x.split_once('/'))
                    && let (Ok(d), Ok(t)) = (d.parse::<u64>(), t.parse::<u64>())
                {
                    prog.total.store(t, Ordering::Relaxed);
                    prog.done.store(d.min(t), Ordering::Relaxed);
                }
            }
            !prog.cancel.load(Ordering::Relaxed)
        });
        let outcome = r.map_err(|e| e.to_string()).and_then(|v| {
            let files: Vec<String> =
                v.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
            let media = files.iter().find(|f| {
                let ext = std::path::Path::new(f).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
                ["mp4", "mov", "webm", "mkv", "png", "jpg", "jpeg", "webp"].contains(&ext.as_str())
            });
            media.cloned().ok_or_else(|| "WanGP produced no video or image".to_string()).map(|path| (path, files))
        });
        let secs = t0.elapsed().as_secs_f64();
        let r = match outcome {
            Ok((path, files)) => {
                set("Shot generated");
                let extra = files.into_iter().filter(|f| *f != path).collect();
                Ok(filmcraft_export::Report { path, frames: 0, seconds: secs, bytes: 0, render_fps: 0.0, extra_files: extra })
            }
            Err(e) => {
                set(&e);
                *prog.error.lock().unwrap_or_else(PoisonError::into_inner) = Some(e.clone());
                Err(e)
            }
        };
        prog.finished.store(true, Ordering::Relaxed);
        *res.lock().unwrap_or_else(PoisonError::into_inner) = Some(r);
    });
    s.jobs.push(job);
    if wait || cfg!(target_arch = "wasm32") {
        run();
        let j = s.jobs.iter().find(|j| j.id == id).map(crate::Job::to_json).unwrap_or(Value::Null);
        return Ok(json!({"job": id, "result": j["result"]}));
    }
    std::thread::Builder::new().name("filmcraft-wangp".into()).spawn(run).map_err(|e| EngineError::Other(e.to_string()))?;
    Ok(json!({"job": id, "poll": "jobs.list", "cancel": "jobs.cancel"}))
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec { id: "ai.status", label: "AI Models Status", menu: &[], shortcut: None, params: "{}", enabled: always, run: status, journal: false },
        CommandSpec {
            id: "ai.generateShot",
            label: "Generate Shot with AI…",
            menu: &[],
            shortcut: None,
            params: r#"{"prompt":str,"seconds":f64?,"duration":str?,"model":str?,"resolution":str?,"profile":str?,"startImage":str?,"wait":bool?}"#,
            enabled: can_generate,
            run: generate_shot,
            journal: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shot_settings_come_from_the_call_then_the_config() {
        let d = std::env::temp_dir().join(format!("filmcraft-ai-tools-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("config.json"),
            r#"{"wangp": {"dossier": "C:/WanGP", "modele": "t2v_1.3B", "duree": "5s", "resolution": "832x480", "profil": "4"}}"#,
        )
        .unwrap();
        let b = Bridge::open(&d).unwrap();
        let _ = std::fs::remove_dir_all(&d);
        let (a, cwd) = shot_args(&b, &json!({"prompt": " a calm sea ", "seconds": 2.0})).unwrap();
        assert_eq!(cwd.as_deref(), Some("C:/WanGP"));
        let pos = |f: &str| a.iter().position(|x| x == f).map(|i| a[i + 1].clone());
        assert_eq!(pos("--prompt").as_deref(), Some("a calm sea"));
        assert_eq!(pos("--duree").as_deref(), Some("2s"));
        assert_eq!(pos("--modele").as_deref(), Some("t2v_1.3B"));
        assert_eq!(pos("--resolution").as_deref(), Some("832x480"));
        let (a, _) = shot_args(&b, &json!({"prompt": "x", "model": "ltx2_distilled"})).unwrap();
        assert!(a.windows(2).any(|w| w[0] == "--duree" && w[1] == "5s"));
        assert!(a.windows(2).any(|w| w[0] == "--modele" && w[1] == "ltx2_distilled"));
        assert!(shot_args(&b, &json!({"prompt": "  "})).is_err());
        assert!(shot_args(&b, &json!({"prompt": "x", "seconds": 90.0})).is_err());
    }

    #[test]
    fn status_without_the_folder() {
        if filmcraft_ai_bridge::find().is_none() {
            let mut s = Session::default();
            let v = s.execute("ai.status", json!({})).unwrap();
            assert_eq!(v["installed"], false);
            assert!(s.execute("ai.generateShot", json!({"prompt": "x"})).is_err());
        }
    }
}
