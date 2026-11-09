"""Envoi des clips sur TikTok par l'API officielle (Content Posting API). Bibliothèque standard uniquement.

    python tiktok_post.py connexion                       # une seule fois : relie ton compte TikTok
    python tiktok_post.py brouillon clip.mp4              # envoie dans tes brouillons TikTok (notification)
    python tiktok_post.py publier clip.mp4 --texte "Titre #gta6 #twitch" [--visibilite PUBLIC_TO_EVERYONE]
    python tiktok_post.py compte                          # vérifie la connexion (pseudo, visibilités permises)

Réglages dans config.json :
    "tiktok": {"client_key": "...", "client_secret": "...", "port": 3455}
La clé et le secret viennent de ton appli sur developers.tiktok.com (ne les partage jamais).
Le jeton de connexion est gardé dans FilmCraft-AI/secrets/tiktok_jeton.json, sur ton PC uniquement.

Rappels TikTok :
  - « brouillon » (droit video.upload) : la vidéo arrive dans ta boîte de réception TikTok ;
    TikTok ne permet pas d'y mettre le texte, tu le colles dans l'appli avant de publier.
  - « publier » (droit video.publish) : tant que l'appli n'a pas passé l'audit de TikTok,
    les vidéos sont forcées en privé (SELF_ONLY).
"""
from __future__ import annotations

import argparse
import hashlib
import http.server
import json
import math
import secrets
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
JETON = ROOT / "secrets" / "tiktok_jeton.json"
API = "https://open.tiktokapis.com"
AUTH = "https://www.tiktok.com/v2/auth/authorize/"
SCOPES = "user.info.basic,video.upload,video.publish"
MO = 1024 * 1024


class TikTokErreur(RuntimeError):
    pass


def log(msg: str) -> None:
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


def reglages() -> dict:
    cfg = json.loads((ROOT / "config.json").read_text(encoding="utf-8")).get("tiktok") or {}
    if not cfg.get("client_key") or not cfg.get("client_secret"):
        raise TikTokErreur('Ajoute "tiktok": {"client_key": "...", "client_secret": "..."} dans config.json.')
    return cfg


def requete(url: str, *, data: bytes | None = None, headers: dict | None = None, method: str = "POST",
            timeout: float = 60) -> tuple[int, dict]:
    req = urllib.request.Request(url, data=data, headers=headers or {}, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            corps = r.read()
            code = r.status
    except urllib.error.HTTPError as e:
        corps = e.read()
        code = e.code
    except (urllib.error.URLError, TimeoutError, OSError) as e:
        raise TikTokErreur(f"réseau : {e}") from e
    try:
        return code, json.loads(corps.decode("utf-8") or "{}")
    except (ValueError, UnicodeDecodeError):
        return code, {"brut": corps[:300].decode("utf-8", "replace")}


# -- connexion (Login Kit pour ordinateur, PKCE) -------------------------------------------------
def connexion() -> dict:
    cfg = reglages()
    port = int(cfg.get("port", 3455))
    redirect = f"http://localhost:{port}/callback/"
    verif = secrets.token_urlsafe(64)[:96]
    # TikTok (ordinateur) attend le SHA-256 en hexadécimal, pas en base64url.
    defi = hashlib.sha256(verif.encode("ascii")).hexdigest()
    etat = secrets.token_urlsafe(16)
    url = AUTH + "?" + urllib.parse.urlencode({
        "client_key": cfg["client_key"], "scope": SCOPES, "redirect_uri": redirect, "state": etat,
        "response_type": "code", "code_challenge": defi, "code_challenge_method": "S256"})
    recu: dict = {}

    class Rappel(http.server.BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            recu.update({k: v[0] for k, v in q.items()})
            ok = "code" in recu and recu.get("state") == etat
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            msg = "Compte TikTok relié, tu peux fermer cet onglet." if ok else "Connexion refusée ou invalide."
            self.wfile.write(f"<h2 style='font-family:sans-serif'>{msg}</h2>".encode("utf-8"))

        def log_message(self, *a):
            pass

    srv = http.server.HTTPServer(("127.0.0.1", port), Rappel)
    srv.timeout = 1
    log("Ouverture de TikTok dans le navigateur : connecte-toi et accepte.")
    webbrowser.open(url)
    fin = time.time() + 300
    while not recu and time.time() < fin:
        srv.handle_request()
    srv.server_close()
    if not recu:
        raise TikTokErreur("pas de réponse de TikTok en 5 min.")
    if recu.get("state") != etat or "code" not in recu:
        raise TikTokErreur(f"connexion refusée : {recu.get('error_description') or recu.get('error') or recu}")
    code, rep = requete(API + "/v2/oauth/token/", headers={"Content-Type": "application/x-www-form-urlencoded"},
                        data=urllib.parse.urlencode({
                            "client_key": cfg["client_key"], "client_secret": cfg["client_secret"],
                            "code": recu["code"], "grant_type": "authorization_code",
                            "redirect_uri": redirect, "code_verifier": verif}).encode())
    return _garder(rep, code)


def _garder(rep: dict, code: int) -> dict:
    if code != 200 or "access_token" not in rep:
        raise TikTokErreur(f"jeton refusé ({code}) : {rep.get('error_description') or rep}")
    now = time.time()
    jeton = {"access_token": rep["access_token"], "refresh_token": rep.get("refresh_token", ""),
             "expire": now + float(rep.get("expires_in", 86400)) - 120,
             "expire_refresh": now + float(rep.get("refresh_expires_in", 31536000)) - 3600,
             "open_id": rep.get("open_id", ""), "scope": rep.get("scope", "")}
    JETON.parent.mkdir(parents=True, exist_ok=True)
    JETON.write_text(json.dumps(jeton, indent=1), encoding="utf-8")
    return jeton


def jeton_valide() -> str:
    if not JETON.exists():
        raise TikTokErreur("compte TikTok pas encore relié : lance « tiktok_post.py connexion ».")
    try:
        j = json.loads(JETON.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise TikTokErreur(f"jeton illisible ({e}) : relance « connexion ».") from e
    if time.time() < float(j.get("expire", 0)):
        return j["access_token"]
    if time.time() > float(j.get("expire_refresh", 0)) or not j.get("refresh_token"):
        raise TikTokErreur("connexion TikTok expirée : relance « tiktok_post.py connexion ».")
    cfg = reglages()
    code, rep = requete(API + "/v2/oauth/token/", headers={"Content-Type": "application/x-www-form-urlencoded"},
                        data=urllib.parse.urlencode({
                            "client_key": cfg["client_key"], "client_secret": cfg["client_secret"],
                            "grant_type": "refresh_token", "refresh_token": j["refresh_token"]}).encode())
    return _garder(rep, code)["access_token"]


def api(chemin: str, corps: dict) -> dict:
    code, rep = requete(API + chemin, data=json.dumps(corps).encode("utf-8"), headers={
        "Authorization": f"Bearer {jeton_valide()}", "Content-Type": "application/json; charset=UTF-8"})
    err = rep.get("error") or {}
    if code != 200 or (err.get("code") not in (None, "ok")):
        raise TikTokErreur(f"{chemin} : {err.get('code') or code} {err.get('message') or rep.get('brut', '')}".strip())
    return rep.get("data") or {}


# -- envoi --------------------------------------------------------------------------------------
def source_fichier(taille: int) -> tuple[dict, int]:
    if taille <= 0:
        raise TikTokErreur("fichier vide.")
    if taille <= 64 * MO:
        morceau = taille                              # un seul envoi
    else:
        morceau = 10 * MO
    nb = max(1, taille // morceau)
    if nb > 1000:
        raise TikTokErreur("vidéo trop grosse pour TikTok.")
    return {"source": "FILE_UPLOAD", "video_size": taille, "chunk_size": morceau, "total_chunk_count": nb}, morceau


def televerser(url: str, video: Path, morceau: int, nb: int) -> None:
    taille = video.stat().st_size
    with open(video, "rb") as f:
        for i in range(nb):
            debut = i * morceau
            fin = taille - 1 if i == nb - 1 else debut + morceau - 1   # le dernier morceau prend le reste
            f.seek(debut)
            data = f.read(fin - debut + 1)
            for essai in range(4):
                code, rep = requete(url, data=data, method="PUT", timeout=300, headers={
                    "Content-Type": "video/mp4", "Content-Length": str(len(data)),
                    "Content-Range": f"bytes {debut}-{fin}/{taille}"})
                if code in (200, 201, 206):
                    break
                if code < 500 or essai == 3:
                    raise TikTokErreur(f"envoi refusé (HTTP {code}) : {rep}")
                time.sleep(2 ** essai)
            log(f"  envoyé {math.floor((fin + 1) * 100 / taille)} %")


def attendre(publish_id: str, cible: str, delai: float = 600) -> dict:
    fin = time.time() + delai
    dernier = {}
    while time.time() < fin:
        dernier = api("/v2/post/publish/status/fetch/", {"publish_id": publish_id})
        st = dernier.get("status")
        if st == "FAILED":
            raise TikTokErreur(f"TikTok a refusé la vidéo : {dernier.get('fail_reason')}")
        if st == cible or st == "PUBLISH_COMPLETE":
            return dernier
        time.sleep(5)
    return dernier


def brouillon(video: Path) -> dict:
    src, morceau = source_fichier(video.stat().st_size)
    data = api("/v2/post/publish/inbox/video/init/", {"source_info": src})
    log(f"Envoi de {video.name} vers tes brouillons TikTok…")
    televerser(data["upload_url"], video, morceau, src["total_chunk_count"])
    etat = attendre(data["publish_id"], "SEND_TO_USER_INBOX")
    log("Dans ta boîte de réception TikTok : ouvre la notification, colle le texte et publie.")
    return {"publish_id": data["publish_id"], "statut": etat.get("status")}


def compte() -> dict:
    return api("/v2/post/publish/creator_info/query/", {})


def publier(video: Path, texte: str, visibilite: str) -> dict:
    info = compte()
    options = info.get("privacy_level_options") or []
    if visibilite not in options:
        raise TikTokErreur(f"visibilité « {visibilite} » non permise pour ce compte/appli : {options}")
    src, morceau = source_fichier(video.stat().st_size)
    post = {"title": texte[:2200], "privacy_level": visibilite,
            "disable_comment": bool(info.get("comment_disabled")), "disable_duet": bool(info.get("duet_disabled")),
            "disable_stitch": bool(info.get("stitch_disabled")),
            "brand_content_toggle": False, "brand_organic_toggle": False}
    data = api("/v2/post/publish/video/init/", {"post_info": post, "source_info": src})
    log(f"Publication de {video.name} sur @{info.get('creator_username', '?')} ({visibilite})…")
    televerser(data["upload_url"], video, morceau, src["total_chunk_count"])
    etat = attendre(data["publish_id"], "PUBLISH_COMPLETE")
    log(f"Statut TikTok : {etat.get('status')}")
    return {"publish_id": data["publish_id"], "statut": etat.get("status")}


def main() -> int:
    ap = argparse.ArgumentParser(description="Envoi des clips FilmCraft sur TikTok (API officielle).")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("connexion")
    sub.add_parser("compte")
    b = sub.add_parser("brouillon")
    b.add_argument("video")
    p = sub.add_parser("publier")
    p.add_argument("video")
    p.add_argument("--texte", default="")
    p.add_argument("--visibilite", default="SELF_ONLY",
                   choices=["PUBLIC_TO_EVERYONE", "MUTUAL_FOLLOW_FRIENDS", "FOLLOWER_OF_CREATOR", "SELF_ONLY"])
    a = ap.parse_args()
    try:
        if a.cmd == "connexion":
            j = connexion()
            log(f"Compte relié (droits : {j.get('scope')}).")
            res: dict = {"ok": True}
        elif a.cmd == "compte":
            res = {"ok": True, **compte()}
            print(json.dumps(res, ensure_ascii=False, indent=1))
        else:
            v = Path(a.video)
            if not v.is_file():
                raise TikTokErreur(f"vidéo introuvable : {v}")
            res = {"ok": True, **(brouillon(v) if a.cmd == "brouillon" else publier(v, a.texte, a.visibilite))}
    except (TikTokErreur, OSError, KeyError) as e:
        print(json.dumps({"ok": False, "error": str(e)}, ensure_ascii=False))
        return 1
    print(json.dumps(res, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
