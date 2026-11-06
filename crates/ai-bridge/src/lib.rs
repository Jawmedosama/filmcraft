//! Bridge to the local **FilmCraft-AI** folder: Python AI models that FilmCraft runs as child
//! processes, each in its own environment.
//!
//! The folder holds `config.json`, one Python environment per model under `envs/` and one worker
//! script per model under `workers/`:
//!
//! | Worker | Model | Used by |
//! |---|---|---|
//! | `whisperx_worker.py` | WhisperX (BSD-2) | `filmcraft-speech`: a [`Transcriber`] for Transcribe, text-based editing and captions |
//! | `chatterbox_worker.py` | Chatterbox (MIT) | `filmcraft-tts`: Chatterbox voices in Text to Speech, with voice cloning |
//! | `wangp_worker.py` | WanGP (WanGP Community License) | the engine's `ai.generateShot` command |
//!
//! [`Transcriber`]: https://docs.rs/ (see `filmcraft_speech::Transcriber`)
//!
//! The folder is found by [`find`]: `FILMCRAFT_AI_DIR`, else `FilmCraft-AI` on the user's desktop.
//! Nothing here needs `unsafe` or a Python binding: workers are plain processes whose last stdout
//! line is a JSON object (`{"ok": true, ...}` or `{"ok": false, "error": "..."}`). A run is
//! cancellable: the caller's progress callback returning `false` kills the child.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

pub mod wav;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BridgeError {
    #[error("FilmCraft-AI is not installed (no config.json in FILMCRAFT_AI_DIR or Desktop\\FilmCraft-AI)")]
    NotInstalled,
    #[error("{0} is not set up: run installer.bat in the FilmCraft-AI folder")]
    MissingEnv(String),
    #[error("stopped")]
    Cancelled,
    #[error("{0}")]
    Worker(String),
    #[error("{0}")]
    Io(String),
}

impl From<std::io::Error> for BridgeError {
    fn from(e: std::io::Error) -> Self {
        BridgeError::Io(e.to_string())
    }
}

/// The three models the folder can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    WhisperX,
    Chatterbox,
    WanGP,
}

impl Model {
    pub fn key(self) -> &'static str {
        match self {
            Model::WhisperX => "whisperx",
            Model::Chatterbox => "chatterbox",
            Model::WanGP => "wangp",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Model::WhisperX => "WhisperX",
            Model::Chatterbox => "Chatterbox",
            Model::WanGP => "WanGP",
        }
    }
    fn worker(self) -> &'static str {
        match self {
            Model::WhisperX => "whisperx_worker.py",
            Model::Chatterbox => "chatterbox_worker.py",
            Model::WanGP => "wangp_worker.py",
        }
    }
}

/// A located FilmCraft-AI folder and its parsed `config.json`.
#[derive(Clone, Debug)]
pub struct Bridge {
    pub root: PathBuf,
    pub config: Value,
}

/// The FilmCraft-AI folder, if one is installed.
pub fn find() -> Option<Bridge> {
    candidates().into_iter().find_map(|d| Bridge::open(&d).ok())
}

fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(d) = std::env::var_os("FILMCRAFT_AI_DIR") {
        v.push(PathBuf::from(d));
    }
    for home in ["USERPROFILE", "HOME"] {
        if let Some(h) = std::env::var_os(home) {
            v.push(PathBuf::from(&h).join("Desktop").join("FilmCraft-AI"));
            v.push(PathBuf::from(&h).join("Bureau").join("FilmCraft-AI"));
        }
    }
    v
}

impl Bridge {
    pub fn open(root: &Path) -> Result<Bridge, BridgeError> {
        let text = std::fs::read_to_string(root.join("config.json")).map_err(|_| BridgeError::NotInstalled)?;
        let config = serde_json::from_str(&text).map_err(|e| BridgeError::Io(format!("config.json: {e}")))?;
        Ok(Bridge { root: root.to_path_buf(), config })
    }

    /// The config section of `model` (`{}` when absent).
    pub fn section(&self, model: Model) -> &Value {
        static EMPTY: Value = Value::Null;
        self.config.get(model.key()).unwrap_or(&EMPTY)
    }

    /// A string setting of `model`, `None` when unset or empty.
    pub fn setting(&self, model: Model, key: &str) -> Option<String> {
        self.section(model).get(key).and_then(|v| match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
    }

    /// The Python interpreter of `model`'s environment.
    pub fn python(&self, model: Model) -> Option<PathBuf> {
        let configured = self.setting(model, "python").map(PathBuf::from);
        let default = if cfg!(windows) {
            self.root.join("envs").join(model.key()).join("Scripts").join("python.exe")
        } else {
            self.root.join("envs").join(model.key()).join("bin").join("python")
        };
        configured.filter(|p| p.is_file()).or_else(|| default.is_file().then_some(default))
    }

    /// `model` can run: its environment and worker exist.
    pub fn available(&self, model: Model) -> bool {
        self.python(model).is_some() && self.root.join("workers").join(model.worker()).is_file()
    }

    /// Where outputs go (`dossier_sortie`, else `<root>/sorties`), created on demand.
    pub fn output_dir(&self) -> Result<PathBuf, BridgeError> {
        let d =
            self.config.get("dossier_sortie").and_then(Value::as_str).filter(|s| !s.is_empty()).map(PathBuf::from).unwrap_or_else(|| self.root.join("sorties"));
        std::fs::create_dir_all(&d)?;
        Ok(d)
    }

    /// A fresh output path `<output dir>/<stem>-<n>.<ext>`.
    pub fn output_path(&self, stem: &str, ext: &str) -> Result<PathBuf, BridgeError> {
        let dir = self.output_dir()?;
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        Ok(dir.join(format!("{stem}-{t}.{ext}")))
    }

    /// Run `model`'s worker with `args`. Lines starting with `[` are progress messages handed to
    /// `progress` (which returns `false` to cancel); the last line is the JSON result.
    pub fn run(&self, model: Model, args: &[String], cwd: Option<&Path>, progress: &mut dyn FnMut(&str) -> bool) -> Result<Value, BridgeError> {
        let python = self.python(model).ok_or_else(|| BridgeError::MissingEnv(model.name().into()))?;
        let script = self.root.join("workers").join(model.worker());
        if !script.is_file() {
            return Err(BridgeError::MissingEnv(model.name().into()));
        }
        let mut cmd = Command::new(python);
        cmd.arg("-u").arg(&script).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        cmd.env("PYTHONIOENCODING", "utf-8").env("PYTHONUTF8", "1");
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        no_window(&mut cmd);
        let mut child = cmd.spawn()?;
        let lines: Arc<Mutex<Vec<String>>> = Arc::default();
        let reader = child.stdout.take().map(|out| {
            let lines = lines.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    lines.lock().unwrap_or_else(PoisonError::into_inner).push(line);
                }
            })
        });
        let mut seen = 0;
        let mut last = String::new();
        let status = loop {
            let fresh: Vec<String> = {
                let l = lines.lock().unwrap_or_else(PoisonError::into_inner);
                l[seen..].to_vec()
            };
            seen += fresh.len();
            let mut keep_going = true;
            for line in fresh {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if line.starts_with('[') {
                    keep_going &= progress(&line);
                }
                last = line;
            }
            if !keep_going || !progress("") {
                let _ = child.kill();
                let _ = child.wait();
                return Err(BridgeError::Cancelled);
            }
            if let Some(s) = child.try_wait()? {
                break s;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        if let Some(r) = reader {
            let _ = r.join();
        }
        let tail: Vec<String> = lines.lock().unwrap_or_else(PoisonError::into_inner)[seen..].to_vec();
        if let Some(l) = tail.iter().rev().map(|l| l.trim()).find(|l| !l.is_empty()) {
            last = l.to_string();
        }
        parse_result(model, &last, status.code())
    }
}

fn parse_result(model: Model, last: &str, code: Option<i32>) -> Result<Value, BridgeError> {
    let v: Value = serde_json::from_str(last).map_err(|_| BridgeError::Worker(format!("{} stopped (exit code {:?}): {last}", model.name(), code)))?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(v)
    } else {
        let e = v.get("error").and_then(Value::as_str).unwrap_or("unknown error");
        Err(BridgeError::Worker(format!("{}: {e}", model.name())))
    }
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_window(_: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results() {
        assert_eq!(parse_result(Model::WanGP, r#"{"ok": true, "files": ["a.mp4"]}"#, Some(0)).unwrap()["files"][0], "a.mp4");
        assert_eq!(parse_result(Model::WanGP, r#"{"ok": false, "error": "no VRAM"}"#, Some(1)), Err(BridgeError::Worker("WanGP: no VRAM".into())));
        assert!(matches!(parse_result(Model::WhisperX, "Traceback", Some(1)), Err(BridgeError::Worker(_))));
    }

    #[test]
    fn config() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(Bridge::open(d.path()).unwrap_err(), BridgeError::NotInstalled);
        std::fs::write(d.path().join("config.json"), r#"{"chatterbox": {"langue": "fr", "voix_reference": ""}, "wangp": {"profil": 4}}"#).unwrap();
        let b = Bridge::open(d.path()).unwrap();
        assert_eq!(b.setting(Model::Chatterbox, "langue").as_deref(), Some("fr"));
        assert_eq!(b.setting(Model::Chatterbox, "voix_reference"), None);
        assert_eq!(b.setting(Model::WanGP, "profil").as_deref(), Some("4"));
        assert!(!b.available(Model::WhisperX));
        assert_eq!(b.run(Model::WhisperX, &[], None, &mut |_| true), Err(BridgeError::MissingEnv("WhisperX".into())));
    }

    #[cfg(unix)]
    #[test]
    fn runs_and_cancels_a_worker() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.json"), "{}").unwrap();
        let env = d.path().join("envs/wangp/bin");
        std::fs::create_dir_all(&env).unwrap();
        std::fs::create_dir_all(d.path().join("workers")).unwrap();
        // a fake "python": a shell that runs the script
        std::fs::write(env.join("python"), "#!/bin/sh\nshift\nexec sh \"$@\"\n").unwrap();
        std::process::Command::new("chmod").arg("+x").arg(env.join("python")).status().unwrap();
        std::fs::write(
            d.path().join("workers/wangp_worker.py"),
            "echo '[wangp] step 1/2'\nif [ \"$1\" = slow ]; then sleep 5; fi\necho '{\"ok\": true, \"files\": [\"x.mp4\"]}'\n",
        )
        .unwrap();
        let b = Bridge::open(d.path()).unwrap();
        let mut seen = Vec::new();
        let r = b.run(Model::WanGP, &[], None, &mut |l| {
            if !l.is_empty() {
                seen.push(l.to_string());
            }
            true
        });
        assert_eq!(r.unwrap()["files"][0], "x.mp4");
        assert_eq!(seen, ["[wangp] step 1/2"]);
        let t0 = std::time::Instant::now();
        let mut n = 0;
        let r = b.run(Model::WanGP, &["slow".into()], None, &mut |_| {
            n += 1;
            n < 3
        });
        assert_eq!(r, Err(BridgeError::Cancelled));
        assert!(t0.elapsed() < Duration::from_secs(4));
    }
}
