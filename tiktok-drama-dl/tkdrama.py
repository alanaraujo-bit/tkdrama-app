#!/usr/bin/env python3
"""tkdrama - baixa novelas verticais (short dramas) do TikTok em fila.

Descobre todos os episodios de uma serie via API do TikTok e baixa cada um
como mp4 progressivo, com retomada, escolha de qualidade e nomeacao ordenada.

Uso:
    py tkdrama.py "https://www.tiktok.com/shortdrama/episode/<dramaID>/1"
    py tkdrama.py <dramaID> --list
    py tkdrama.py <dramaID> --episodes 1-10 --out "D:/Novelas"
"""
from __future__ import annotations

import argparse
import http.cookiejar
import gzip
import json
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36 Edg/152.0.0.0")
API = "https://www.tiktok.com/api/drama/episode/item_list/"
PAGE = 24
CHUNK = 262144

AUTH_HINT = (
    "Sessao do TikTok invalida ou expirada.\n"
    "  Reexporte o cookies.txt: abra tiktok.com logado, use a extensao\n"
    "  'Get cookies.txt LOCALLY' e salve por cima de cookies.txt (formato Netscape)."
)


class Fatal(Exception):
    """Erro esperado: mostra mensagem limpa, sem traceback."""


# --------------------------------------------------------------------------- util

def safe_name(s):
    s = re.sub(r'[\\/:*?"<>|]', "", s or "")
    s = re.sub(r"\s+", " ", s).strip().rstrip(".")
    return s or "drama"


def human(n):
    for unit in ("B", "KB", "MB", "GB"):
        if n < 1024:
            return f"{n:.0f}{unit}" if unit == "B" else f"{n:.1f}{unit}"
        n /= 1024
    return f"{n:.1f}TB"


def parse_drama_id(raw):
    raw = (raw or "").strip().strip('"')
    if re.fullmatch(r"\d{6,}", raw):
        return raw
    for pat in (r"/shortdrama/(?:episode|detail)/(\d+)", r"[?&]dramaID=(\d+)"):
        m = re.search(pat, raw)
        if m:
            return m.group(1)
    raise Fatal(
        f"Nao consegui achar o ID da serie em: {raw}\n"
        "  Esperado algo como https://www.tiktok.com/shortdrama/episode/<ID>/1"
    )


def parse_ranges(spec, lo, hi):
    """'1-10,15,20-25' -> {1..10, 15, 20..25}"""
    out = set()
    for part in (spec or "").split(","):
        part = part.strip()
        if not part:
            continue
        m = re.fullmatch(r"(\d+)\s*-\s*(\d+)", part)
        if m:
            a, b = int(m.group(1)), int(m.group(2))
            out.update(range(min(a, b), max(a, b) + 1))
        elif part.isdigit():
            out.add(int(part))
        else:
            raise Fatal(f"Intervalo invalido em --episodes: {part!r}")
    return {n for n in out if lo <= n <= hi}


# ------------------------------------------------------------------------ cookies

def load_cookies(path):
    if not os.path.isfile(path):
        raise Fatal(f"Arquivo de cookies nao encontrado: {path}\n\n{AUTH_HINT}")

    lines = ["# Netscape HTTP Cookie File"]
    kept = 0
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh.read().splitlines():
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            f = line.split("\t")
            if len(f) < 7 or "tiktok" not in f[0]:
                continue
            # Extensoes gravam '.dominio' com flag FALSE; o parser do Python exige
            # que a flag combine com o ponto inicial.
            f[1] = "TRUE" if f[0].startswith(".") else "FALSE"
            lines.append("\t".join(f))
            kept += 1

    if not kept:
        raise Fatal(f"Nenhum cookie de tiktok.com em {path}\n\n{AUTH_HINT}")

    tmp = path + ".norm"
    with open(tmp, "w", encoding="utf-8", newline="\n") as fh:
        fh.write("\n".join(lines) + "\n")
    try:
        jar = http.cookiejar.MozillaCookieJar(tmp)
        jar.load(ignore_discard=True, ignore_expires=True)
    finally:
        try:
            os.remove(tmp)
        except OSError:
            pass

    if "sessionid" not in {c.name for c in jar}:
        print("  aviso: cookie 'sessionid' ausente; episodios podem falhar.",
              file=sys.stderr)
    return jar


# ---------------------------------------------------------------------------- api

def build_opener(jar):
    return urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))


def _read(resp):
    data = resp.read()
    if resp.headers.get("Content-Encoding") == "gzip":
        data = gzip.decompress(data)
    return data.decode("utf-8", "replace")


def api_page(op, drama_id, cursor):
    params = {
        "aid": "1988", "app_language": "pt-BR", "app_name": "tiktok_web",
        "browser_language": "pt-BR", "browser_name": "Mozilla",
        "browser_online": "true", "browser_platform": "Win32",
        "browser_version": UA, "channel": "tiktok_web", "cookie_enabled": "true",
        "coverFormat": "2", "device_platform": "web_pc", "dramaID": drama_id,
        "language": "pt-BR", "os": "windows", "priority_region": "BR",
        "region": "BR", "screen_height": "1080", "screen_width": "1920",
        "tz_name": "America/Sao_Paulo", "webcast_language": "pt-BR",
        "count": str(PAGE), "cursor": str(cursor),
    }
    url = API + "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={
        "User-Agent": UA,
        "Accept": "application/json, text/plain, */*",
        "Accept-Language": "pt-BR,pt;q=0.9",
        "Accept-Encoding": "gzip",
        "Referer": f"https://www.tiktok.com/shortdrama/episode/{drama_id}/1",
    })
    try:
        body = _read(op.open(req, timeout=60))
    except urllib.error.HTTPError as e:
        raise Fatal(f"API respondeu HTTP {e.code}.\n\n{AUTH_HINT}") from None
    except urllib.error.URLError as e:
        raise Fatal(f"Falha de rede ao consultar a API: {e.reason}") from None

    if not body.strip():
        raise Fatal(
            "API respondeu vazio. Isso quer dizer sessao morta ou que o TikTok\n"
            f"passou a exigir assinatura nessa rota.\n\n{AUTH_HINT}"
        )
    try:
        data = json.loads(body)
    except json.JSONDecodeError:
        raise Fatal(f"API respondeu algo que nao e JSON.\n\n{AUTH_HINT}") from None

    status = data.get("statusCode", data.get("status_code"))
    if status not in (0, None):
        raise Fatal(
            f"API recusou (statusCode={status}): "
            f"{data.get('status_msg') or 'sem detalhe'}\n\n{AUTH_HINT}"
        )
    return data


def fetch_episodes(op, drama_id, delay):
    """Retorna ({numero: item}, dramaInfo)."""
    episodes = {}
    info = {}
    cursor, expected, guard = 0, None, 0

    while True:
        guard += 1
        if guard > 200:
            raise Fatal("Paginacao nao terminou; abortando por seguranca.")
        data = api_page(op, drama_id, cursor)
        items = data.get("itemList") or []
        if expected is None:
            expected = data.get("totalEpisodeCount")
        for it in items:
            dvd = (it.get("dramaInfo") or {}).get("DramaVideoData") or {}
            num = dvd.get("EpisodeNumber")
            if isinstance(num, int) and num > 0:
                episodes[num] = it
            if not info:
                info = it.get("dramaInfo") or {}
        print(f"  lista: {len(episodes)} episodios"
              + (f" de {expected}" if expected else ""), end="\r", flush=True)
        if not data.get("hasMore") or not items:
            break
        nxt = data.get("cursor")
        cursor = int(nxt) if str(nxt).isdigit() else cursor + len(items)
        time.sleep(delay)

    print(" " * 60, end="\r")
    if not episodes:
        raise Fatal(f"Nenhum episodio retornado para a serie {drama_id}.\n\n{AUTH_HINT}")
    return episodes, info


# -------------------------------------------------------------------------- gears

def pick_gear(item, codec, gear_name):
    """Escolhe a variante de qualidade. None = episodio sem midia acessivel."""
    infos = (item.get("video") or {}).get("bitrateInfo") or []
    usable = [b for b in infos if ((b.get("PlayAddr") or {}).get("UrlList"))]
    if not usable:
        return None
    if gear_name:
        for b in usable:
            if b.get("GearName") == gear_name:
                return b
        return max(usable, key=lambda b: b.get("Bitrate") or 0)
    preferred = [b for b in usable if str(b.get("CodecType") or "").startswith(codec)]
    return max(preferred or usable, key=lambda b: b.get("Bitrate") or 0)


# ----------------------------------------------------------------------- download

def _stream(op, url, part, expected, label):
    have = os.path.getsize(part) if os.path.exists(part) else 0
    if expected and have > expected:
        os.remove(part)
        have = 0

    headers = {"User-Agent": UA, "Referer": "https://www.tiktok.com/", "Accept": "*/*"}
    if have:
        headers["Range"] = f"bytes={have}-"
    resp = op.open(urllib.request.Request(url, headers=headers), timeout=60)

    total = None
    cr = resp.headers.get("Content-Range")
    if cr and "/" in cr:
        tail = cr.rsplit("/", 1)[-1].strip()
        total = int(tail) if tail.isdigit() else None
    elif resp.headers.get("Content-Length"):
        total = int(resp.headers["Content-Length"]) + have

    # URL reassinada pode apontar para outra variante: nao emendar por cima.
    if expected and total and total != expected:
        resp.close()
        if os.path.exists(part):
            os.remove(part)
        raise IOError(f"tamanho inesperado ({total} != {expected}); reiniciando")

    append = bool(have) and resp.status == 206
    if not append:
        have = 0
    written = have
    goal = total or expected or 0

    with open(part, "ab" if append else "wb") as fh:
        while True:
            buf = resp.read(CHUNK)
            if not buf:
                break
            fh.write(buf)
            written += len(buf)
            if goal:
                pct = written * 100 // goal
                print(f"    {label} {pct:3d}%  {human(written)}/{human(goal)}",
                      end="\r", flush=True)
    resp.close()

    if goal and written != goal:
        raise IOError(f"incompleto: {written}/{goal}")
    return written


def download_episode(op, gear, dest, label):
    urls = list((gear.get("PlayAddr") or {}).get("UrlList") or [])
    expected = int((gear.get("PlayAddr") or {}).get("DataSize") or 0)
    part = dest + ".part"
    last = None

    for i, url in enumerate(urls, 1):
        for attempt in (1, 2):
            try:
                size = _stream(op, url, part, expected, label)
                os.replace(part, dest)
                return size
            except urllib.error.HTTPError as e:
                last = e
                if e.code in (403, 401):
                    break  # URL assinada morreu: vai para o proximo CDN
            except Exception as e:
                last = e
                if attempt == 1:
                    time.sleep(2)
        if i < len(urls):
            print(f"    {label} CDN {i} falhou ({type(last).__name__}); tentando o proximo")

    raise IOError(f"todas as {len(urls)} URLs falharam: {last}")


# --------------------------------------------------------------------------- main

def run(args):
    drama_id = parse_drama_id(args.url)
    jar = load_cookies(args.cookies)
    op = build_opener(jar)

    print(f"Serie {drama_id}: consultando episodios...")
    episodes, info = fetch_episodes(op, drama_id, args.delay)
    name = safe_name(info.get("dramaName") or f"drama-{drama_id}")
    nums = sorted(episodes)
    print(f'"{name}" - {len(nums)} episodios (E{nums[0]:02d}-E{nums[-1]:02d})')

    wanted = parse_ranges(args.episodes, nums[0], nums[-1]) if args.episodes else set(nums)
    todo = [n for n in nums if n in wanted]
    if not todo:
        raise Fatal("Nenhum episodio bateu com o filtro --episodes.")

    if args.list:
        print(f"\n{'Ep':>4}  {'Qualidade':<22} {'Codec':<10} {'Res':<12} {'Tam':>9}  Dur")
        print("-" * 74)
        for n in todo:
            g = pick_gear(episodes[n], args.codec, args.gear)
            if not g:
                print(f"{n:>4}  (sem midia acessivel)")
                continue
            pa = g["PlayAddr"]
            dur = (episodes[n].get("video") or {}).get("duration") or 0
            res = f"{pa.get('Width')}x{pa.get('Height')}"
            print(f"{n:>4}  {g.get('GearName', ''):<22} {g.get('CodecType', ''):<10} "
                  f"{res:<12} {human(pa.get('DataSize') or 0):>9}  {dur}s")
        return 0

    outdir = args.out or os.path.join(os.getcwd(), name)
    os.makedirs(outdir, exist_ok=True)

    manifest = {
        "dramaID": drama_id,
        "dramaName": info.get("dramaName"),
        "totalEpisodes": len(nums),
        "fetchedAt": time.strftime("%Y-%m-%d %H:%M:%S"),
        "episodes": {},
    }

    done = skipped = failed = 0
    locked = []
    errors = []

    for idx, n in enumerate(todo, 1):
        item = episodes[n]
        gear = pick_gear(item, args.codec, args.gear)
        label = f"[{idx}/{len(todo)}] E{n:02d}"

        if gear is None:
            locked.append(n)
            print(f"    {label} sem midia acessivel (bloqueado) - pulado")
            continue

        pa = gear["PlayAddr"]
        expected = int(pa.get("DataSize") or 0)
        dest = os.path.join(outdir, f"{name} - E{n:02d}.mp4")
        manifest["episodes"][str(n)] = {
            "itemId": item.get("id"),
            "gear": gear.get("GearName"),
            "codec": gear.get("CodecType"),
            "width": pa.get("Width"),
            "height": pa.get("Height"),
            "size": expected,
            "duration": (item.get("video") or {}).get("duration"),
            "file": os.path.basename(dest),
        }

        if os.path.exists(dest) and (not expected or os.path.getsize(dest) == expected):
            skipped += 1
            print(f"    {label} ja existe - pulado")
            continue

        try:
            size = download_episode(op, gear, dest, label)
            done += 1
            print(f"    {label} OK  {human(size)}  {pa.get('Width')}x{pa.get('Height')}"
                  f"  {gear.get('GearName')}" + " " * 12)
        except Fatal:
            raise
        except Exception as e:
            failed += 1
            errors.append(f"E{n:02d}: {e}")
            print(f"    {label} FALHOU: {e}" + " " * 12)

        if idx < len(todo):
            time.sleep(args.delay)

    with open(os.path.join(outdir, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, ensure_ascii=False, indent=1)

    print(f"\nPasta: {outdir}")
    print(f"Baixados: {done} | Ja existiam: {skipped} | "
          f"Falhas: {failed} | Bloqueados: {len(locked)}")
    if locked:
        print("  Sem midia acessivel (provavelmente exigem desbloqueio): "
              + ", ".join(f"E{n:02d}" for n in locked))
    for err in errors:
        print(f"  falha {err}")
    if failed:
        print("  Rode o mesmo comando de novo: os episodios prontos sao pulados e as")
        print("  falhas recomecam com URLs novas.")
    return 1 if failed else 0


def main():
    ap = argparse.ArgumentParser(
        prog="tkdrama",
        description="Baixa novelas verticais (short dramas) do TikTok em fila.")
    ap.add_argument("url", help="URL da serie (/shortdrama/episode/<ID>/1) ou o ID puro")
    ap.add_argument("-o", "--out", help="pasta de destino (padrao: ./<nome da serie>)")
    ap.add_argument("-c", "--cookies", default="cookies.txt",
                    help="cookies.txt em formato Netscape (padrao: cookies.txt)")
    ap.add_argument("-e", "--episodes", help="filtro, ex: 1-10,15,20-25")
    ap.add_argument("--codec", default="h264", choices=["h264", "h265"],
                    help="codec preferido (padrao: h264, compativel com tudo)")
    ap.add_argument("--gear", help="forca uma variante exata, ex: adapt_lowest_1080_1")
    ap.add_argument("--delay", type=float, default=1.0,
                    help="pausa em segundos entre requisicoes (padrao: 1.0)")
    ap.add_argument("-l", "--list", action="store_true",
                    help="so lista os episodios e as qualidades, sem baixar")
    args = ap.parse_args()

    try:
        return run(args)
    except Fatal as e:
        print(f"\nErro: {e}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print("\nInterrompido. Rode de novo para retomar de onde parou.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
