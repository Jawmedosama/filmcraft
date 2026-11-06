# AI Studio: local AI models

FilmCraft can use three AI models installed in a local **FilmCraft-AI** folder. Each runs in its
own Python environment as a child process; FilmCraft itself stays pure Rust (`crates/ai-bridge`
starts the processes, no FFI, no `unsafe`).

| Model | Licence | Where it shows up |
|---|---|---|
| WhisperX | BSD-2-Clause | Speech model `whisperx` (Settings ▸ Media Analysis & Transcription), Transcribe, the Text panel's transcript, text-based editing and captions |
| Chatterbox | MIT | Text to Speech: Chatterbox voices in English, French, Spanish, German and Italian, plus a cloned voice per language |
| WanGP | WanGP Community License | AI Studio panel ▸ Generate a shot; command `ai.generateShot` |

**WanGP's licence** allows free use, but WanGP may not be embedded in a paid product or offered as
a paid service without a commercial licence, and products that use it must say so in their
interface and documentation. The AI Studio panel shows this notice.

## Finding the folder

`FILMCRAFT_AI_DIR`, else `FilmCraft-AI` on the Desktop (`Desktop` or `Bureau`). The folder holds:

- `config.json`: per-model settings (`whisperx.modele`, `whisperx.langue`, `chatterbox.voix_reference`,
  `wangp.dossier`, `wangp.modele`, `wangp.duree`, `wangp.resolution`, `wangp.profil`, `dossier_sortie`).
- `envs/<model>/`: one Python environment per model (`installer.bat` creates them).
- `workers/<model>_worker.py`: one script per model. Lines starting with `[` are progress; the last
  stdout line is `{"ok": true, ...}` or `{"ok": false, "error": "..."}`.

## Behaviour

- **Transcription** uses WhisperX when `whisperx` is the speech model, or when the built-in Whisper
  can't run (not built in, or its model not downloaded) and WhisperX is installed. Speakers are
  labelled by FilmCraft's own diarization, so no Hugging Face token is needed.
- **Chatterbox voices** synthesize in the Text to Speech background job. Pause markers work; pace is
  applied by resampling; the pitch control is ignored. The cloned voices need
  `chatterbox.voix_reference` (a 5 to 10 s WAV of clean speech).
- **Generate a shot** runs as a job (`jobs.list`, `jobs.cancel`); its `result.path` is the generated
  file. The panel imports it and places it on a new video track at the playhead.

## Commands

| Command | Params | Result |
|---|---|---|
| `ai.status` | – | `{installed, dir, whisperx, chatterbox, wangp, wangpModel, wangpNotice}` |
| `ai.generateShot` | `{prompt, seconds?, duration?, model?, resolution?, profile?, startImage?, wait?}` | `{job}` (or `{job, result}` with `wait`) |
