"""Clippeur de live Twitch en quasi direct pour FilmCraft IA.

    python twitch_live.py <chaine> [--nombre-max 20] [--seuil 3.0] [--qualite 720p60,720p,best]
    python twitch_live.py <chaine> --apres-live     # détecte pendant le live, monte les clips à la fin
    python twitch_live.py --test                     # vérifie la détection sur des données synthétiques

Principe :
  1. streamlink enregistre le live sans réencodage dans sorties/live-<chaine>-<date>/live.ts ;
     le même flux est décodé en audio mono 8 kHz pour mesurer le volume seconde par seconde
     (ce décodage donne aussi l'horloge exacte de l'enregistrement).
  2. Le chat est lu en anonyme (IRC Twitch, aucun compte) : messages par seconde, emotes de rire
     ou de hype, « clip ».
  3. Un moment est déclenché quand le chat et/ou le son s'emballent nettement par rapport aux
     5 dernières minutes. On découpe ~1 min 30 autour (sans réencodage) et on lance
     le pipeline « Clips viraux » existant dessus (WhisperX + Qwen3 + recadrage), un moment à la fois.
  4. index.html dans le dossier du live liste les moments au fur et à mesure.

Bibliothèque standard uniquement. streamlink et ffmpeg viennent de l'environnement envs\\clips
(pip install streamlink ; ffmpeg via imageio-ffmpeg déjà installé).
Ctrl+C arrête l'enregistrement puis termine les clips en attente ; un deuxième Ctrl+C abandonne.
"""
from __future__ import annotations

import argparse
import html
import json
import math
import os
import queue
import random
import re
import socket
import statistics
import subprocess
import sys
import threading
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent

AUDIO_RATE = 8000  # Hz, mono s16le : assez pour un volume, très léger
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)

# Réactions qui signalent un moment drôle / impressionnant / à clipper (comparées en minuscules).
REACTIONS = {
    "kekw", "lul", "lulw", "omegalul", "lmao", "lmfao", "xd", "mdr", "ptdr", "jpp", "ahah", "haha",
    "pog", "pogchamp", "poggers", "pogu", "pogo", "ez", "gg", "wtf", "omg", "hype", "monkas",
    "pepelaugh", "icant", "aware", "clip", "clipped", "clippez", "clippe", "+1", "w", "😂", "🤣", "💀", "🔥",
}
RE_RIRE = re.compile(r"^(a?h[ae]){2,}h?$|^x+d+$|^m+d+r+$|^p+t+d+r+$|^l+o+l+$|^k+e+k+w*$")


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def load_config() -> dict:
    return json.loads((ROOT / "config.json").read_text(encoding="utf-8"))


def reaction_count(text: str) -> int:
    n = 0
    for tok in text.lower().split():
        tok = tok.strip(".,!?:;\"'()")
        if tok in REACTIONS or RE_RIRE.match(tok or "-"):
            n += 1
    return min(n, 1)  # on compte des personnes qui réagissent, pas des emotes répétées


# -- détection ----------------------------------------------------------------------------------
@dataclass
class Moment:
    debut: float          # secondes dans l'enregistrement : début du pic
    fin: float            # fin du pic
    pic: float            # seconde du score max
    score: float
    chat: float           # messages/s au pic
    raisons: list[str] = field(default_factory=list)
    statut: str = "en attente"
    extrait: str = ""
    dossier: str = ""
    galerie: str = ""


def _robust(values: list[float]) -> tuple[float, float]:
    """Médiane et écart robuste (MAD), avec un plancher pour éviter les divisions par ~0."""
    if not values:
        return 0.0, 1.0
    med = statistics.median(values)
    mad = statistics.median([abs(v - med) for v in values]) * 1.4826
    return med, mad


class Detecteur:
    """Reçoit des secondes (messages, réactions, volume) et sort des moments forts.

    Scores en « écarts robustes » par rapport aux 5 dernières minutes, pour s'adapter à la taille
    de la chaîne (5 messages/s n'est rien chez un gros streamer, énorme chez un petit).
    """

    def __init__(self, seuil: float = 3.5, base_s: int = 300, fenetre_chat: int = 8,
                 fenetre_son: int = 3, pause: float = 60.0, chauffe: float = 90.0, duree_min: int = 3):
        self.seuil = seuil
        self.base_s = base_s
        self.fc = fenetre_chat
        self.fs = fenetre_son
        self.pause = pause
        self.chauffe = chauffe
        self.duree_min = duree_min
        self.chat: dict[int, int] = {}
        self.reac: dict[int, int] = {}
        self.son: dict[int, float] = {}
        self.auteurs: dict[int, set[str]] = {}
        self.hist_chat: deque[float] = deque(maxlen=base_s)
        self.hist_reac: deque[float] = deque(maxlen=base_s)
        self.hist_son: deque[float] = deque(maxlen=base_s)
        self.t = -1                 # dernière seconde évaluée
        self.en_cours: Moment | None = None
        self.dernier_fin = -1e9

    def message(self, t: float, texte: str, auteur: str = "") -> None:
        if texte.lstrip().startswith("!"):   # commandes de bot (!giveaway, !discord…) : pas une réaction
            return
        s = int(t)
        if s <= self.t:            # message en retard : on le range dans la seconde courante
            s = self.t + 1
        # Une personne compte au plus une fois par seconde : le spam d'un seul viewer ne fait pas un pic.
        vus = self.auteurs.setdefault(s, set())
        if auteur and auteur in vus:
            return
        if auteur:
            vus.add(auteur)
        self.chat[s] = self.chat.get(s, 0) + 1
        self.reac[s] = self.reac.get(s, 0) + reaction_count(texte)

    def volume(self, s: int, db: float) -> None:
        self.son[s] = db

    def _somme(self, d: dict[int, float], s: int, w: int) -> float:
        return sum(d.get(i, 0) for i in range(s - w + 1, s + 1)) / w

    def evaluer(self, jusqua: int) -> list[Moment]:
        """Évalue chaque seconde jusqu'à `jusqua` (exclu) ; renvoie les moments terminés."""
        sortis: list[Moment] = []
        for s in range(self.t + 1, jusqua):
            self.t = s
            c = self._somme(self.chat, s, self.fc)
            r = self._somme(self.reac, s, self.fc)
            sons = [self.son[i] for i in range(s - self.fs + 1, s + 1) if i in self.son]
            a = sum(sons) / len(sons) if sons else None

            mc, dc = _robust(list(self.hist_chat))
            mr, dr = _robust(list(self.hist_reac))
            z_chat = (c - mc) / max(dc, 0.25 * mc, 0.5) if self.hist_chat else 0.0
            z_reac = (r - mr) / max(dr, 0.25 * mr, 0.4) if self.hist_reac else 0.0
            z_son = 0.0
            if a is not None and len(self.hist_son) > 30:
                ma, da = _robust(list(self.hist_son))
                z_son = (a - ma) / max(da, 1.5)
            # Le chat est le meilleur signal ; le son seul déclenche s'il est vraiment fort.
            score = 0.55 * max(z_chat, 0) + 0.45 * max(z_reac, 0) + 0.35 * max(z_son, 0)
            if c < 0.2 and z_son < self.seuil * 1.5:  # chat quasi mort : on exige un son exceptionnel
                score = min(score, 0.35 * max(z_son, 0) / 1.5)

            actif = s >= self.chauffe and score >= self.seuil
            if self.en_cours is None:
                if actif and s - self.dernier_fin >= self.pause:
                    raisons = []
                    if z_chat >= 2:
                        raisons.append("chat")
                    if z_reac >= 2:
                        raisons.append("réactions")
                    if z_son >= 2:
                        raisons.append("son")
                    self.en_cours = Moment(debut=s, fin=s, pic=s, score=score, chat=c, raisons=raisons)
            else:
                m = self.en_cours
                if score >= self.seuil * 0.75:      # hystérésis : le pic continue
                    m.fin = s
                    if score > m.score:
                        m.score, m.pic, m.chat = score, s, c
                    for nom, z in (("chat", z_chat), ("réactions", z_reac), ("son", z_son)):
                        if z >= 2 and nom not in m.raisons:
                            m.raisons.append(nom)
                if s - m.fin > 4 or s - m.debut > 120:
                    if m.fin - m.debut >= self.duree_min:   # un éclair d'1-2 s, c'est du bruit
                        sortis.append(m)
                        self.dernier_fin = m.fin
                    self.en_cours = None
            # Ligne de base : on n'y met pas les secondes de pic, sinon elle « apprend » l'emballement.
            if self.en_cours is None:
                self.hist_chat.append(c)
                self.hist_reac.append(r)
                if a is not None:
                    self.hist_son.append(a)
            for d in (self.chat, self.reac, self.son, self.auteurs):
                d.pop(s - max(self.fc, self.fs) - 2, None)
        return sortis

    def vider(self) -> list[Moment]:
        if self.en_cours is None:
            return []
        m, self.en_cours = self.en_cours, None
        return [m] if m.fin - m.debut >= self.duree_min else []


# -- sources -------------------------------------------------------------------------------------
class Horloge:
    """Relie l'heure murale à la position dans l'enregistrement (donnée par l'audio décodé)."""

    def __init__(self):
        self.lock = threading.Lock()
        self.points: deque[tuple[float, float]] = deque(maxlen=600)

    def marquer(self, media_s: float) -> None:
        with self.lock:
            self.points.append((time.time(), media_s))

    def media(self, mur: float | None = None) -> float | None:
        mur = time.time() if mur is None else mur
        with self.lock:
            if not self.points:
                return None
            w, m = self.points[-1]
        return max(0.0, m + (mur - w))

    def duree(self) -> float:
        with self.lock:
            return self.points[-1][1] if self.points else 0.0


def lire_chat(chaine: str, sortie: "queue.Queue[tuple[float, str, str]]", stop: threading.Event) -> None:
    """Chat Twitch en lecture seule, connexion anonyme (justinfanNNNN), reconnexion automatique."""
    attente = 2.0
    while not stop.is_set():
        try:
            with socket.create_connection(("irc.chat.twitch.tv", 6667), timeout=30) as sk:
                sk.settimeout(1.0)
                nick = f"justinfan{random.randint(10000, 99999)}"
                sk.sendall(f"CAP REQ :twitch.tv/tags\r\nNICK {nick}\r\nJOIN #{chaine}\r\n".encode())
                log(f"Chat connecté : #{chaine}")
                attente = 2.0
                buf = b""
                dernier = time.time()
                while not stop.is_set():
                    try:
                        data = sk.recv(65536)
                    except socket.timeout:
                        if time.time() - dernier > 330:
                            raise ConnectionError("chat silencieux depuis 5 min")
                        continue
                    if not data:
                        raise ConnectionError("connexion fermée")
                    dernier = time.time()
                    buf += data
                    *lignes, buf = buf.split(b"\r\n")
                    for brut in lignes:
                        ligne = brut.decode("utf-8", "replace")
                        if ligne.startswith("PING"):
                            sk.sendall(b"PONG :tmi.twitch.tv\r\n")
                            continue
                        if " PRIVMSG #" not in ligne:
                            continue
                        mur = time.time()
                        if ligne.startswith("@"):
                            m = re.search(r"(?:^@|;)tmi-sent-ts=(\d+)", ligne)
                            if m:
                                mur = int(m.group(1)) / 1000.0
                        ma = re.search(r":([A-Za-z0-9_]+)![^ ]* PRIVMSG #", ligne)
                        texte = ligne.split(" PRIVMSG #", 1)[1].split(" :", 1)
                        sortie.put((mur, texte[1] if len(texte) > 1 else "", ma.group(1) if ma else ""))
        except (OSError, ConnectionError) as e:
            if stop.is_set():
                break
            log(f"Chat déconnecté ({e}), nouvelle tentative dans {attente:.0f} s")
            stop.wait(attente)
            attente = min(attente * 2, 60)


def ffmpeg_exe(cfg: dict) -> str:
    py = cfg.get("clips", {}).get("python", "")
    if py and Path(py).exists():
        try:
            out = subprocess.run([py, "-c", "import imageio_ffmpeg;print(imageio_ffmpeg.get_ffmpeg_exe())"],
                                 capture_output=True, text=True, timeout=60, creationflags=NO_WINDOW)
            exe = out.stdout.strip().splitlines()[-1] if out.stdout.strip() else ""
            if exe and Path(exe).exists():
                return exe
        except (OSError, subprocess.SubprocessError):
            pass
    return "ffmpeg"


def streamlink_cmd(cfg: dict) -> list[str]:
    py = cfg.get("clips", {}).get("python", "")
    if py and Path(py).exists():
        return [py, "-m", "streamlink"]
    return ["streamlink"]


class Enregistreur:
    """streamlink -> live.ts (copie) + ffmpeg -> PCM pour le volume et l'horloge."""

    def __init__(self, cfg: dict, chaine: str, qualite: str, fichier: Path, ffmpeg: str,
                 horloge: Horloge, volumes: "queue.Queue[tuple[int, float]]"):
        self.fichier = fichier
        self.horloge = horloge
        self.volumes = volumes
        cmd = streamlink_cmd(cfg) + ["--stdout", "--twitch-disable-ads", "--loglevel", "warning",
                                     f"https://www.twitch.tv/{chaine}", qualite]
        self.sl = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, creationflags=NO_WINDOW)
        self.ff = subprocess.Popen([ffmpeg, "-hide_banner", "-loglevel", "error", "-i", "pipe:0", "-vn",
                                    "-ac", "1", "-ar", str(AUDIO_RATE), "-f", "s16le", "pipe:1"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                   creationflags=NO_WINDOW)
        self.octets = 0
        self.erreur = ""
        self.t_copie = threading.Thread(target=self._copier, daemon=True)
        self.t_audio = threading.Thread(target=self._audio, daemon=True)
        self.t_err = threading.Thread(target=self._stderr, daemon=True)
        self.t_copie.start()
        self.t_audio.start()
        self.t_err.start()

    def _stderr(self) -> None:
        assert self.sl.stderr is not None
        for ligne in self.sl.stderr:
            txt = ligne.decode("utf-8", "replace").strip()
            if txt:
                self.erreur = txt
                log(f"streamlink : {txt}")

    def _copier(self) -> None:
        assert self.sl.stdout is not None and self.ff.stdin is not None
        with open(self.fichier, "wb") as f:
            while True:
                bloc = self.sl.stdout.read(188 * 1024)
                if not bloc:
                    break
                f.write(bloc)
                f.flush()
                self.octets += len(bloc)
                try:
                    self.ff.stdin.write(bloc)
                except (BrokenPipeError, OSError):
                    pass
        try:
            self.ff.stdin.close()
        except OSError:
            pass

    def _audio(self) -> None:
        assert self.ff.stdout is not None
        par_s = AUDIO_RATE * 2
        s = 0
        while True:
            bloc = self.ff.stdout.read(par_s)
            if len(bloc) < par_s:
                break
            n = len(bloc) // 2
            somme = 0
            for i in range(0, len(bloc) - 1, 16):  # 1 échantillon sur 8 : largement assez pour un volume
                v = int.from_bytes(bloc[i:i + 2], "little", signed=True)
                somme += v * v
            rms = math.sqrt(somme / max(1, n // 8)) / 32768.0
            db = 20 * math.log10(max(rms, 1e-5))
            self.volumes.put((s, db))
            s += 1
            self.horloge.marquer(float(s))

    def vivant(self) -> bool:
        return self.t_copie.is_alive() or self.t_audio.is_alive()

    def arreter(self) -> None:
        for p in (self.sl, self.ff):
            if p.poll() is None:
                p.terminate()
        for p in (self.sl, self.ff):
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()


# -- montage ------------------------------------------------------------------------------------
def couper(ffmpeg: str, src: Path, debut: float, duree: float, dest: Path) -> None:
    cmd = [ffmpeg, "-hide_banner", "-loglevel", "error", "-y", "-ss", f"{debut:.2f}", "-i", str(src),
           "-t", f"{duree:.2f}", "-map", "0:v:0?", "-map", "0:a:0?", "-c", "copy",
           "-avoid_negative_ts", "make_zero", "-movflags", "+faststart", str(dest)]
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=300, creationflags=NO_WINDOW)
    if r.returncode != 0 or not dest.exists() or dest.stat().st_size < 10000:
        raise RuntimeError(f"découpe impossible : {r.stderr.strip()[-300:]}")


def lancer_clips(extrait: Path, m: Moment, args) -> tuple[str, str]:
    consigne = ("Extrait d'un live Twitch. Le chat a réagi fortement vers "
                f"{max(0, m.pic - m.debut + args.avant):.0f} s ({', '.join(m.raisons) or 'pic'}). "
                "Choisis le passage qui contient la cause de cette réaction, avec sa mise en place.")
    cmd = [sys.executable, "-u", str(HERE / "ai_hub.py"), "clips", str(extrait), "--nombre", "1",
           "--duree-min", str(args.duree_min), "--duree-max", str(args.duree_max),
           "--consigne", consigne, "--ne-pas-ouvrir"]
    if args.titre:
        cmd.append("--titre")
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8",
                         errors="replace", env={**os.environ, "PYTHONUTF8": "1"}, creationflags=NO_WINDOW)
    assert p.stdout is not None
    dossier, derniere = "", ""
    for ligne in p.stdout:
        ligne = ligne.rstrip()
        if not ligne:
            continue
        derniere = ligne
        mm = re.search(r"clips dans (.+)$", ligne)
        if mm:
            dossier = mm.group(1).strip()
        if args.bavard:
            log(f"    {ligne}")
    p.wait()
    if p.returncode != 0 or not dossier:
        raise RuntimeError(derniere or f"code {p.returncode}")
    page = Path(dossier) / "index.html"
    return str(Path(dossier).resolve()), str(page.resolve()) if page.exists() else ""


def ecrire_index(dossier: Path, chaine: str, moments: list[Moment], en_direct: bool) -> None:
    try:
        _ecrire_index(dossier, chaine, moments, en_direct)
    except (OSError, ValueError) as e:  # le récapitulatif ne doit jamais arrêter l'enregistrement
        log(f"Récapitulatif non écrit : {e}")


def _ecrire_index(dossier: Path, chaine: str, moments: list[Moment], en_direct: bool) -> None:
    lignes = []
    for i, m in enumerate(sorted(moments, key=lambda x: x.debut), 1):
        lien = f'<a href="{html.escape(Path(m.galerie).resolve().as_uri())}">voir le clip</a>' if m.galerie else ""
        lignes.append(
            f"<tr><td>{i}</td><td>{hms(m.debut)}</td><td>{m.score:.1f}</td><td>{m.chat:.1f}/s</td>"
            f"<td>{html.escape(', '.join(m.raisons))}</td><td>{html.escape(m.statut)}</td><td>{lien}</td></tr>")
    rafraichir = '<meta http-equiv="refresh" content="30">' if en_direct else ""
    (dossier / "index.html").write_text(
        f"""<!doctype html><html lang="fr"><head><meta charset="utf-8">{rafraichir}
<title>Live {html.escape(chaine)}</title><style>
body{{font-family:system-ui,sans-serif;background:#16141c;color:#eee;margin:24px}}
table{{border-collapse:collapse}}td,th{{padding:6px 12px;border-bottom:1px solid #333;text-align:left}}
a{{color:#b48cff}}</style></head><body>
<h1>Live {html.escape(chaine)} {'(en cours)' if en_direct else '(terminé)'}</h1>
<p>{len(moments)} moments détectés. La page se met à jour toute seule pendant le live.</p>
<table><tr><th>#</th><th>Moment</th><th>Score</th><th>Chat</th><th>Pourquoi</th><th>État</th><th></th></tr>
{''.join(lignes)}</table></body></html>""", encoding="utf-8")
    (dossier / "moments.json").write_text(
        json.dumps([m.__dict__ for m in moments], ensure_ascii=False, indent=1), encoding="utf-8")


def hms(s: float) -> str:
    s = int(s)
    return f"{s // 3600}:{s % 3600 // 60:02d}:{s % 60:02d}"


# -- boucle principale --------------------------------------------------------------------------
def live(args) -> int:
    cfg = load_config()
    chaine = args.chaine.lower().lstrip("#").split("/")[-1]
    sortie = Path(cfg.get("dossier_sortie") or ROOT / "sorties")
    dossier = sortie / f"live-{chaine}-{time.strftime('%Y%m%d-%H%M%S')}"
    dossier.mkdir(parents=True, exist_ok=True)
    fichier = dossier / "live.ts"
    ffmpeg = ffmpeg_exe(cfg)

    stop = threading.Event()
    horloge = Horloge()
    msgs: queue.Queue[tuple[float, str, str]] = queue.Queue()
    vols: queue.Queue[tuple[int, float]] = queue.Queue()
    det = Detecteur(seuil=args.seuil)
    moments: list[Moment] = []
    a_monter: queue.Queue[Moment] = queue.Queue()
    abandon = threading.Event()

    log(f"Enregistrement de twitch.tv/{chaine} dans {dossier}")
    rec = Enregistreur(cfg, chaine, args.qualite, fichier, ffmpeg, horloge, vols)
    threading.Thread(target=lire_chat, args=(chaine, msgs, stop), daemon=True).start()

    def monteur() -> None:
        while True:
            m = a_monter.get()
            if m is None or abandon.is_set():
                return
            debut = max(0.0, m.debut - args.avant)
            fin = min(max(m.fin, m.pic) + args.apres, debut + 180)
            while not args.apres_live and horloge.duree() < fin + 5 and rec.vivant() and not abandon.is_set():
                time.sleep(2)
            fin = min(fin, horloge.duree())
            if fin - debut < args.duree_min + 5:
                m.statut = "trop court"
                ecrire_index(dossier, chaine, moments, rec.vivant())
                continue
            try:
                m.statut = "découpe"
                ext = dossier / f"moment-{int(m.debut):06d}.mp4"
                couper(ffmpeg, fichier, debut, fin - debut, ext)
                m.extrait = str(ext)
                m.statut = "montage IA"
                ecrire_index(dossier, chaine, moments, rec.vivant())
                log(f"Montage du moment {hms(m.debut)} (score {m.score:.1f})…")
                t0 = time.time()
                m.dossier, m.galerie = lancer_clips(ext, m, args)
                m.statut = "prêt"
                log(f"Clip prêt en {(time.time() - t0) / 60:.1f} min : {m.dossier}")
            except Exception as e:  # un moment raté ne doit pas arrêter le live
                m.statut = f"erreur : {e}"[:200]
                log(f"Moment {hms(m.debut)} : {m.statut}")
            ecrire_index(dossier, chaine, moments, rec.vivant())

    t_monteur = threading.Thread(target=monteur, daemon=True)
    t_monteur.start()
    en_attente: list[Moment] = []  # mode --apres-live

    def accepter(nouveaux: list[Moment]) -> None:
        for m in nouveaux:
            garde = [x for x in moments if x.statut != "remplacé"]
            if len(garde) >= args.nombre_max:
                # en mode après-live on peut encore échanger contre un meilleur moment pas encore monté
                attente = [x for x in garde if x in en_attente and x.statut == "en attente"]
                pire = min(attente, key=lambda x: x.score) if attente else None
                if pire is None or m.score <= pire.score:
                    log(f"Moment {hms(m.debut)} ignoré (limite de {args.nombre_max} atteinte)")
                    continue
                pire.statut = "remplacé"
            moments.append(m)
            log(f"MOMENT FORT à {hms(m.debut)} : score {m.score:.1f}, chat {m.chat:.1f} msg/s "
                f"({', '.join(m.raisons) or 'pic'})")
            if args.apres_live:
                en_attente.append(m)
            else:
                a_monter.put(m)
        if nouveaux:
            ecrire_index(dossier, chaine, moments, True)

    ecrire_index(dossier, chaine, moments, True)
    dernier_etat = time.time()
    try:
        while rec.vivant():
            time.sleep(1)
            while not vols.empty():
                s, db = vols.get_nowait()
                det.volume(s, db)
            while not msgs.empty():
                mur, txt, qui = msgs.get_nowait()
                t = horloge.media(mur)
                if t is not None:
                    det.message(t, txt, qui)
            # on évalue avec 3 s de marge pour laisser arriver le son et les messages
            accepter(det.evaluer(int(horloge.duree()) - 3))
            if time.time() - dernier_etat > 300:
                dernier_etat = time.time()
                log(f"En cours : {hms(horloge.duree())} enregistrées, {rec.octets / 1e9:.2f} Go, "
                    f"{len(moments)} moments")
        log("Fin du live (ou flux interrompu).")
        if rec.erreur and horloge.duree() < 5:
            log("Rien n'a été enregistré : la chaîne est-elle en direct ? Le nom est-il exact ?")
    except KeyboardInterrupt:
        log("Arrêt demandé : fin de l'enregistrement, les clips en attente vont être montés "
            "(Ctrl+C encore pour tout abandonner).")
    finally:
        stop.set()
        rec.arreter()

    try:
        accepter(det.evaluer(int(horloge.duree())) + det.vider())
        for m in sorted(en_attente, key=lambda x: -x.score):
            if m.statut == "en attente":
                a_monter.put(m)
        a_monter.put(None)  # type: ignore[arg-type]
        while t_monteur.is_alive():
            t_monteur.join(1)
    except KeyboardInterrupt:
        abandon.set()
        log("Abandon des clips restants.")
    ecrire_index(dossier, chaine, moments, False)
    prets = sum(1 for m in moments if m.statut == "prêt")
    log(f"Terminé : {prets}/{len(moments)} clips prêts. Récapitulatif : {dossier / 'index.html'}")
    return 0


# -- test hors ligne ----------------------------------------------------------------------------
def test() -> int:
    """Live synthétique de 20 min : chat de fond bruité + 3 vrais moments + un pic de spam isolé."""
    rnd = random.Random(7)
    det = Detecteur()
    vrais = {300: "chat", 700: "son", 1000: "rires"}
    trouves: list[Moment] = []
    for s in range(1200):
        base = 2.0 + 0.8 * math.sin(s / 90)
        n = sum(1 for _ in range(int(base * 2)) if rnd.random() < 0.5)
        db = -30 + rnd.gauss(0, 2)
        for t0, kind in vrais.items():
            if t0 <= s < t0 + 15:
                if kind == "chat":
                    n += 12
                elif kind == "rires":
                    n += 6
                elif kind == "son":
                    db = -10
        for _ in range(n):
            txt = "KEKW KEKW" if (vrais.get(1000) and 1000 <= s < 1015) or (300 <= s < 315 and rnd.random() < .5) \
                else rnd.choice(["salut", "gg", "ok", "c'est quoi ce jeu", "mdr", "?", "trop bien"])
            det.message(s + rnd.random(), txt, f"viewer{rnd.randrange(400)}")
        if 500 <= s < 510:  # un seul viewer qui spamme, et un bot de commandes : pas des moments
            for _ in range(15):
                det.message(s + .5, "KEKW KEKW KEKW", "spammeur")
                det.message(s + .5, "!giveaway", f"viewer{rnd.randrange(400)}")
        det.volume(s, db)
        trouves += det.evaluer(s - 2)
    trouves += det.evaluer(1200) + det.vider()
    for m in trouves:
        print(f"  moment {hms(m.debut)}  pic {hms(m.pic)}  score {m.score:.1f}  {', '.join(m.raisons)}")
    attendus = sorted(vrais)
    ok = len(trouves) == len(attendus) and all(abs(m.debut - t) <= 12 for m, t in zip(trouves, attendus))
    print("OK" if ok else f"ÉCHEC : attendu des moments vers {attendus}")
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser(description="Clippe automatiquement un live Twitch en quasi direct.")
    ap.add_argument("chaine", nargs="?", help="nom de la chaîne ou lien twitch.tv/…")
    ap.add_argument("--qualite", default="720p60,720p,best", help="qualités streamlink, par ordre de préférence")
    ap.add_argument("--seuil", type=float, default=3.5, help="sensibilité : plus bas = plus de moments")
    ap.add_argument("--nombre-max", type=int, default=20, help="nombre maximum de moments par live")
    ap.add_argument("--avant", type=float, default=60, help="secondes gardées avant la réaction du chat")
    ap.add_argument("--apres", type=float, default=25, help="secondes gardées après la fin de la réaction")
    ap.add_argument("--duree-min", type=float, default=15)
    ap.add_argument("--duree-max", type=float, default=60)
    ap.add_argument("--titre", action="store_true", help="titre accrocheur sur les clips")
    ap.add_argument("--apres-live", action="store_true", help="ne monte les clips qu'à la fin du live (GPU libre)")
    ap.add_argument("--bavard", action="store_true", help="affiche le détail du montage IA")
    ap.add_argument("--test", action="store_true", help="teste la détection sur un live synthétique")
    a = ap.parse_args()
    if a.test:
        return test()
    if not a.chaine:
        ap.error("indique le nom de la chaîne, par exemple : python twitch_live.py zerator")
    return live(a)


if __name__ == "__main__":
    sys.exit(main())
