"""
MuseScore PDF worker.

MuseScore serves free community scores only as per-page images behind a
Cloudflare Turnstile challenge and CORS-less cross-origin CDNs, so they cannot
be fetched server-side by a normal HTTP client. This worker drives a real
(nodriver) Chrome that passes the challenge, then reconstructs the score.

## How pages are fetched (the robust, Xvfb-independent method)

The MuseScore viewer loads page 0 from a preloaded, signed URL in the page
HTML. Pages 1..N-1 are fetched on demand from a token-gated same-origin API,
`GET /api/jmuse?id=<id>&index=<n>&type=img`, whose `Authorization` header is:

    md5(f"{id}img{index}{SALT}")[:4]

`SALT` is a build constant embedded in a MuseScore JS bundle, right before a
`.substr(0,4)` call (e.g. `(e+t+r+"61256397").substr(0,4)`). We recover it at
runtime by regex over the loaded bundles, and keep known-good constants as
fallbacks. jmuse returns JSON `{info:{url: <signed CDN url>}}`.

An earlier version drove the on-page viewer and screenshotted each page as it
scrolled into view. That relied on the viewer's scroll-triggered lazy-load,
which fires on a real display but NOT under Xvfb-in-Docker (page 0 loaded,
pages 1..N never did — see HANDOFF.md). The jmuse method sidesteps the viewer
entirely: for each page we mint the token, call jmuse *from inside the
challenge-passed page* (so the request carries the valid cf_clearance cookie +
Chrome fingerprint — a plain server-side client gets 403/CAPTCHA), inject the
returned image URL as a full-size `<img>`, and screenshot it. No scroll, no
IntersectionObserver, no virtualization — so it is immune to the Xvfb
lazy-load failure.

HTTP API:
    GET /healthz         -> "ok"
    GET /pdf/<score_id>  -> application/pdf  (or 4xx/5xx + JSON error; the 422
                            body embeds a short diag so failures are
                            diagnosable from the response alone)

Env:
    PORT   (default 8000)
    WINDOW (default "1500,1000")   host window size for headful Chrome
    PAGE_W (default 1400)          CSS width each page image is rendered at
    PAGE_SCALE (default 2)         device scale factor => screenshot DPI
    HEADLESS (default 0)           1 = headless (does NOT clear the challenge;
                                   for debugging only)
    HARVEST_TIMEOUT, CDP_CALL_TIMEOUT   whole-harvest / per-CDP-call ceilings

Design notes:
  * One shared browser; requests are serialized with an asyncio.Lock because a
    single Chrome can only sensibly render one score at a time. Each request
    uses its own tab, closed at the end.
  * Everything Cloudflare-sensitive happens inside the browser; the only bytes
    that leave are the finished page PNGs / PDF.
"""

import asyncio
import base64
import hashlib
import json
import os
import shutil
import subprocess
import time
import nodriver as uc
from nodriver import cdp
from aiohttp import web
import img2pdf

PAGE_W = int(os.environ.get("PAGE_W", "1400"))
PAGE_SCALE = int(os.environ.get("PAGE_SCALE", "2"))
NAV_TIMEOUT = 60          # seconds to wait for the score page + challenge
MAX_PAGES = 60            # sanity cap
HARVEST_TIMEOUT = int(os.environ.get("HARVEST_TIMEOUT", "360"))
CDP_CALL_TIMEOUT = int(os.environ.get("CDP_CALL_TIMEOUT", "25"))

# Known-good jmuse salts, tried after any runtime-extracted candidates. The
# first is the current build constant (extracted 2026-07-06); the second is
# dl-librescore's long-standing fallback. Each score self-validates the salt
# (the first that makes jmuse return a URL wins), so a stale constant is
# harmless as long as extraction OR one of these still works.
FALLBACK_SALTS = ["61256397", "9654,4e"]


def log(msg: str):
    print(f"[worker] {msg}", flush=True)


def mint_token(score_id: str, index: int, salt: str) -> str:
    """The /api/jmuse Authorization token: md5(id + 'img' + index + salt)[:4]."""
    return hashlib.md5(f"{score_id}img{index}{salt}".encode()).hexdigest()[:4]


PAGES_JS = ("(window.UGAPP&&UGAPP.store&&UGAPP.store.page&&UGAPP.store.page.data"
            "&&UGAPP.store.page.data.score)?UGAPP.store.page.data.score.pages_count:0")

# Recover candidate salts from the loaded MuseScore bundles. The token-minting
# code looks like `(e+t+r+"<salt>").substr(0,4)`; grab the quoted literal
# immediately before a `.substr(0,4)` / `.slice(0,4)`. Same-origin bundles only
# (cross-origin ad/analytics scripts CORS-block the fetch and are irrelevant).
EXTRACT_SALTS_JS = r"""async () => {
  const urls = new Set();
  document.querySelectorAll('script[src]').forEach(s => urls.add(s.src));
  performance.getEntriesByType('resource').map(e => e.name)
    .forEach(u => { if (/\.js(\?|$)/.test(u)) urls.add(u); });
  const ms = [...urls].filter(u => { try { return new URL(u).host === location.host; } catch (e) { return false; } });
  const re = /["'`]([^"'`]{2,24})["'`]\s*\)\s*\.\s*(?:substr|substring|slice)\s*\(\s*0\s*,\s*4\s*\)/g;
  const out = [];
  for (const u of ms) {
    let t = '';
    try { t = await (await fetch(u)).text(); } catch (e) { continue; }
    let m;
    while ((m = re.exec(t)) !== null) { if (!out.includes(m[1])) out.push(m[1]); }
  }
  return JSON.stringify(out);
}"""


def jmuse_fetch_js(score_id: str, index: int, token: str) -> str:
    """In-page fetch of the signed page URL. Runs inside the challenge-passed
    page so it carries the valid session; a server-side fetch would 403."""
    return (r"""async () => {
  try {
    const r = await fetch('/api/jmuse?id=%ID%&index=%IDX%&type=img',
                          {headers: {Authorization: '%TOK%'}});
    if (!r.ok) return JSON.stringify({ok:false, status:r.status});
    const j = await r.json();
    const url = j && j.info && j.info.url;
    return JSON.stringify({ok: !!url, status:r.status, url: url || null});
  } catch (e) { return JSON.stringify({ok:false, err:String(e)}); }
}""").replace("%ID%", score_id).replace("%IDX%", str(index)).replace("%TOK%", token)


def show_page_js(url: str, width: int) -> str:
    """Replace the page with a white full-bleed overlay holding just this page
    image at `width` CSS px, wait for it to decode, and return its rect (doc
    coords, scroll reset to 0) for a beyond-viewport screenshot."""
    return (r"""async () => {
  window.scrollTo(0, 0);
  let ov = document.getElementById('__cap_overlay');
  if (!ov) {
    ov = document.createElement('div');
    ov.id = '__cap_overlay';
    ov.style.cssText = 'position:absolute;top:0;left:0;z-index:2147483647;margin:0;padding:0;background:#fff;';
    document.body.appendChild(ov);
  }
  ov.innerHTML = '';
  // white backdrop so any transparent PNG/SVG pixels render white, not the body
  const img = document.createElement('img');
  img.style.cssText = 'display:block;width:%W%px;height:auto;background:#fff;';
  ov.appendChild(img);
  const done = new Promise((res) => {
    img.onload = () => res(true);
    img.onerror = () => res(false);
  });
  img.src = %URL%;
  const ok = await Promise.race([done, new Promise(r => setTimeout(() => r('timeout'), 25000))]);
  try { await img.decode(); } catch (e) {}
  const r = img.getBoundingClientRect();
  return JSON.stringify({ok, w: Math.round(r.width), h: Math.round(r.height),
                         nw: img.naturalWidth, nh: img.naturalHeight});
}""").replace("%W%", str(int(width))).replace("%URL%", json.dumps(url))

# Page 0 is preloaded; its signed URL is in the <link rel=preload as=image> (or
# the first score_0 image). Use it directly rather than a jmuse call.
PAGE0_URL_JS = r"""(() => {
  for (const l of document.querySelectorAll('link[as="image"]')) {
    if (/\/score_0\./i.test(l.href || '')) return l.href;
  }
  for (const i of document.querySelectorAll('img')) {
    const s = i.currentSrc || i.src || '';
    if (/scoredata\/g\/[0-9a-f]+\/score_0\./i.test(s)) return s;
  }
  return '';
})()"""


def _unwrap(r):
    return r["value"] if isinstance(r, dict) and "type" in r and "value" in r else r


async def _ev(tab, expr, ap=False):
    return _unwrap(await asyncio.wait_for(tab.evaluate(expr, await_promise=ap),
                                          timeout=CDP_CALL_TIMEOUT))


async def _ejson(tab, expr, ap=False):
    s = await _ev(tab, expr, ap)
    return json.loads(s) if isinstance(s, str) else s


async def _wait_for(tab, expr, tries=NAV_TIMEOUT):
    for _ in range(tries):
        try:
            r = await _ev(tab, expr)
            if r:
                return r
        except Exception:
            pass
        await asyncio.sleep(1)
    return None


async def _resolve_salt(tab, score_id, pc):
    """Find a salt that makes jmuse return a URL. Probe index is 1 when the
    score has >1 page (page 0 doesn't need jmuse); else 0. Extracted-from-bundle
    candidates first, then the known-good constants."""
    probe = 1 if pc > 1 else 0
    try:
        extracted = await _ejson(tab, f"({EXTRACT_SALTS_JS})()", ap=True) or []
    except Exception as e:
        log(f"salt extraction failed: {e}")
        extracted = []
    candidates = list(dict.fromkeys([*extracted, *FALLBACK_SALTS]))
    log(f"score {score_id}: {len(extracted)} extracted salt(s), trying {len(candidates)} total")
    for salt in candidates:
        tok = mint_token(score_id, probe, salt)
        try:
            r = await _ejson(tab, f"({jmuse_fetch_js(score_id, probe, tok)})()", ap=True)
        except Exception:
            continue
        if r.get("ok") and r.get("url"):
            src = "extracted" if salt in extracted else "fallback"
            log(f"score {score_id}: salt OK ({src}, len={len(salt)})")
            return salt
        log(f"score {score_id}: salt candidate rejected (jmuse status={r.get('status')})")
    return None


async def _page_url(tab, score_id, index, salt):
    if index == 0:
        u = await _ev(tab, PAGE0_URL_JS)
        if u:
            return u
    tok = mint_token(score_id, index, salt)
    r = await _ejson(tab, f"({jmuse_fetch_js(score_id, index, tok)})()", ap=True)
    return r.get("url") if r.get("ok") else None


async def _capture_page(tab, url):
    """Inject the page image, screenshot it at PAGE_SCALE, return PNG bytes."""
    info = await _ejson(tab, f"({show_page_js(url, PAGE_W)})()", ap=True)
    if not info.get("ok") or not info.get("w"):
        return None
    vp = cdp.page.Viewport(x=0.0, y=0.0, width=float(info["w"]),
                           height=float(info["h"]), scale=1.0)
    data = await asyncio.wait_for(
        tab.send(cdp.page.capture_screenshot(
            format_="png", clip=vp, capture_beyond_viewport=True)),
        timeout=CDP_CALL_TIMEOUT)
    raw = base64.b64decode(data)
    return raw if len(raw) > 3000 else None


async def harvest_pages(browser, score_id: str):
    """Return per-page PNG bytes for `score_id`, in order. Raises RuntimeError
    with a short reason (challenge not cleared, paywalled, salt broken, ...)."""
    log(f"score {score_id}: navigating to score page")
    tab = await browser.get(f"https://musescore.com/score/{score_id}")
    try:
        pc = await _wait_for(tab, PAGES_JS)
        if not pc:
            raise RuntimeError("no score data (challenge not cleared or not a score)")
        pc = int(pc)
        if pc < 1 or pc > MAX_PAGES:
            raise RuntimeError(f"implausible pages_count={pc}")
        log(f"score {score_id}: challenge cleared, pages_count={pc}")

        # Render each page image at a fixed width; a page is taller than the
        # window, so beyond-viewport capture is used. device_scale_factor gives
        # print-quality DPI. Height is generous; the clip is per-page exact.
        try:
            await asyncio.wait_for(tab.send(cdp.emulation.set_device_metrics_override(
                width=PAGE_W + 120, height=2200, device_scale_factor=PAGE_SCALE,
                mobile=False)), timeout=CDP_CALL_TIMEOUT)
        except Exception as e:
            log(f"score {score_id}: set_device_metrics_override failed: {e}")

        salt = None
        if pc > 1:
            salt = await _resolve_salt(tab, score_id, pc)
            if not salt:
                raise RuntimeError("could not resolve jmuse salt (all candidates rejected)")

        pages: dict[int, bytes] = {}
        for n in range(pc):
            url = await _page_url(tab, score_id, n, salt)
            if not url:
                raise RuntimeError(f"no image URL for page {n} (jmuse rejected)")
            png = await _capture_page(tab, url)
            if not png:
                raise RuntimeError(f"failed to capture page {n}")
            pages[n] = png
            log(f"score {score_id}: captured page {n} ({len(png)} bytes) [{len(pages)}/{pc}]")

        log(f"score {score_id}: all {pc} pages captured")
        return [pages[n] for n in range(pc)]
    finally:
        try:
            await tab.close()
        except Exception:
            pass


def chrome_binary():
    p = os.environ.get("CHROME_PATH")
    if p and os.path.exists(p):
        return p
    for name in ("google-chrome", "google-chrome-stable", "chrome"):
        w = shutil.which(name)
        if w:
            return w
    return None


def chrome_stderr_probe(args):
    """Launch Chrome directly for a few seconds and capture its stderr.
    nodriver pipes (and never reads) the browser's stderr, so a failed launch
    reports only a generic error; this recovers the real reason."""
    exe = chrome_binary()
    if not exe:
        return "chrome binary not found on PATH"
    probe_args = [exe, *args, "--user-data-dir=/tmp/chrome-probe",
                  "--remote-debugging-port=9223", "about:blank"]
    try:
        p = subprocess.run(probe_args, capture_output=True, text=True, timeout=8)
        return f"exit={p.returncode} :: {((p.stderr or '') + (p.stdout or '')).strip()[:1200]}"
    except subprocess.TimeoutExpired:
        return "probe survived 8s (launch OK)"
    except Exception as e:
        return f"probe failed to run: {e}"


class Worker:
    def __init__(self):
        self.browser = None
        self.lock = asyncio.Lock()

    def _chrome_args(self):
        win = os.environ.get("WINDOW", "1500,1000")
        args = [
            f"--window-size={win}",
            # No window manager under xvfb-run => Chrome may treat its window as
            # unfocused/occluded and throttle rendering. Harmless on a desktop.
            "--disable-background-timer-throttling",
            "--disable-backgrounding-occluded-windows",
            "--disable-renderer-backgrounding",
            "--disable-features=CalculateNativeWinOcclusion",
        ]
        args.extend(os.environ.get("CHROME_EXTRA_ARGS", "").split())
        return args

    async def get_browser(self):
        if self.browser is None:
            headless = os.environ.get("HEADLESS", "0") == "1"
            args = self._chrome_args()
            exe = chrome_binary()
            if exe:
                try:
                    v = subprocess.run([exe, "--version"], capture_output=True,
                                       text=True, timeout=10).stdout.strip()
                    log(f"chrome binary: {exe} ({v})")
                except Exception:
                    pass
            # Pin Chrome via CHROME_PATH (a Chrome-for-Testing build) when set,
            # so the container isn't at the mercy of `google-chrome-stable`
            # auto-bumping to a version nodriver can't drive (Chrome 150 broke
            # the launch/CDP handshake under Xvfb; 149 is stable).
            chrome_path = os.environ.get("CHROME_PATH") or None
            last_err = None
            for attempt in (1, 2, 3):
                log(f"starting Chrome (attempt {attempt}, headless={headless}, path={chrome_path}, args={args})")
                try:
                    self.browser = await uc.start(
                        headless=headless, browser_args=args,
                        browser_executable_path=chrome_path)
                    log("Chrome started")
                    return self.browser
                except Exception as e:
                    last_err = e
                    log(f"Chrome start attempt {attempt} failed: {type(e).__name__}: {e}")
                    await asyncio.sleep(2 * attempt)
            probe = chrome_stderr_probe(args)
            log(f"chrome stderr probe: {probe}")
            raise RuntimeError(
                f"chrome failed to start after 3 attempts: {last_err} | probe: {probe}")
        return self.browser

    def reset_browser(self):
        """Drop the shared browser so the next request starts a fresh one.
        Called after a timeout or CDP/WebSocket error: a cancelled harvest
        leaves Chrome unusable. `browser.stop()` is synchronous in nodriver."""
        b = self.browser
        self.browser = None
        if b is not None:
            try:
                b.stop()
            except Exception:
                pass
        log("browser reset (will relaunch on next request)")

    async def handle_pdf(self, request: web.Request) -> web.Response:
        score_id = request.match_info["score_id"]
        if not score_id.isdigit():
            return web.json_response({"error": "score_id must be numeric"}, status=400)
        async with self.lock:
            try:
                browser = await self.get_browser()
                pages = await asyncio.wait_for(
                    harvest_pages(browser, score_id), timeout=HARVEST_TIMEOUT)
            except asyncio.TimeoutError:
                log(f"score {score_id}: harvest timed out after {HARVEST_TIMEOUT}s")
                self.reset_browser()
                return web.json_response({"error": "harvest timed out", "chrome": chrome_diag()}, status=504)
            except RuntimeError as e:
                # Expected "can't get this score" -> 422 so the caller falls back
                # to linking out. Reset the browser anyway: a challenge that
                # didn't clear (or a paywalled score) leaves the session in a
                # state where the NEXT request's navigation dies with "no close
                # frame received or sent" on the reused connection. A fresh
                # browser per failed request also gives the Turnstile a clean
                # retry instead of compounding a flagged session.
                log(f"score {score_id}: unharvestable: {e}")
                self.reset_browser()
                return web.json_response({"error": str(e), "chrome": chrome_diag()}, status=422)
            except Exception as e:
                log(f"score {score_id}: worker error: {e}")
                self.reset_browser()
                return web.json_response({"error": f"worker error: {e}", "chrome": chrome_diag()}, status=500)
        try:
            pdf = img2pdf.convert(pages)
        except Exception as e:
            return web.json_response({"error": f"pdf assembly failed: {e}"}, status=500)
        return web.Response(body=pdf, content_type="application/pdf",
                            headers={"Content-Disposition": f'inline; filename="musescore-{score_id}.pdf"'})


async def healthz(_request):
    return web.Response(text="ok")


def chrome_diag() -> dict:
    """What Chrome the worker will actually drive — so a failure is diagnosable
    from an HTTP response alone (no container logs / Portainer needed)."""
    path = chrome_binary()
    env_path = os.environ.get("CHROME_PATH")
    d = {"resolved": path, "CHROME_PATH": env_path,
         "CHROME_PATH_exists": bool(env_path and os.path.exists(env_path))}
    if path:
        try:
            d["version"] = subprocess.run([path, "--version"], capture_output=True,
                                          text=True, timeout=10).stdout.strip()
        except Exception as e:
            d["version_error"] = str(e)
    return d


async def debug(_request):
    return web.json_response({
        "chrome": chrome_diag(),
        "env": {k: os.environ.get(k) for k in
                ("CHROME_EXTRA_ARGS", "HEADLESS", "PAGE_W", "PAGE_SCALE",
                 "HARVEST_TIMEOUT", "WINDOW")},
    })


def main():
    worker = Worker()
    app = web.Application(client_max_size=64 * 1024 * 1024)
    app.router.add_get("/healthz", healthz)
    app.router.add_get("/debug", debug)
    app.router.add_get("/pdf/{score_id}", worker.handle_pdf)
    port = int(os.environ.get("PORT", "8000"))
    web.run_app(app, host="0.0.0.0", port=port)


if __name__ == "__main__":
    main()
