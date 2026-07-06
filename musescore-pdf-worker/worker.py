"""
MuseScore PDF worker.

MuseScore serves free community scores only as per-page images behind a
Cloudflare Turnstile challenge and CORS-less cross-origin CDNs, so they cannot
be fetched server-side by a normal HTTP client. This worker drives a real
(nodriver) Chrome that passes the challenge, brings each page of the score
viewer into view, and screenshots each rendered page at ~200 DPI, then
stitches the pages into a PDF.

HTTP API (FlareSolverr-independent, dead simple):
    GET /healthz         -> "ok"
    GET /pdf/<score_id>  -> application/pdf  (or 4xx/5xx + JSON error; the 422
                            body embeds the last DIAG snapshot so failures are
                            diagnosable from the response alone, no container
                            logs needed)

Env:
    PORT   (default 8000)
    WINDOW (default "1500,1000")   host window size for headful Chrome
    VP_W, VP_H, VP_SCALE           emulated viewport for nav (default 1400x2200 @2x)
    VP_H_MAX                       max chunk viewport height (default 3800; at
                                   @2x that stays under SwiftShader's 8192-px
                                   texture ceiling in GPU-less containers)
    PASSES                         capture passes over the score (default 3)
    CHUNK_WAIT                     max seconds to wait per chunk for lazy-loaded
                                   pages to appear + decode (default 14)
    HARVEST_TIMEOUT, CDP_CALL_TIMEOUT   whole-harvest / per-CDP-call ceilings

Design notes:
  * One shared browser; requests are serialized with an asyncio.Lock because a
    single Chrome can only sensibly render one score at a time (and it keeps the
    memory footprint bounded). Each request uses its own tab, closed at the end.
  * Everything Cloudflare-sensitive happens inside the browser; the only bytes
    that leave are the finished page PNGs / PDF.
  * The score viewer VIRTUALIZES pages: only pages near the viewport exist in
    the DOM, lazy-loaded as they scroll into view. Under Xvfb-in-Docker the
    scroll-triggered lazy-load proved unreliable (page 0 loaded, pages 1..N
    never materialized — see HANDOFF.md), so the harvest attacks it from
    several sides at once:
      - Chrome launches with the render/timer throttling for unfocused or
        occluded windows disabled (there is no window manager under xvfb-run,
        so Chrome may consider the window unfocused forever).
      - CDP focus emulation + bringToFront so the page believes it is the
        active, focused tab (IntersectionObserver + rAF-driven viewers idle
        otherwise).
      - The emulated viewport is resized to fit several pages at once (from
        the scroller's real scrollHeight), so far fewer lazy-load triggers are
        needed, and each chunk position is reached with a native scrollTop
        write PLUS dispatched scroll events PLUS a real CDP mouse-wheel event
        (the input pipeline path a human scroll takes).
"""

import asyncio
import base64
import json
import os
import shutil
import subprocess
import time
import nodriver as uc
from nodriver import cdp
from aiohttp import web
import img2pdf

VP_W = int(os.environ.get("VP_W", "1400"))
VP_H = int(os.environ.get("VP_H", "2200"))
VP_SCALE = int(os.environ.get("VP_SCALE", "2"))
# Max height of the per-chunk emulated viewport. 3800 CSS px @2x = 7600 device
# px, under the 8192 texture ceiling of SwiftShader (the software GL used in a
# GPU-less container). Taller would risk blank/failed captures in Docker.
VP_H_MAX = int(os.environ.get("VP_H_MAX", "3800"))
PASSES = int(os.environ.get("PASSES", "3"))
CHUNK_WAIT = float(os.environ.get("CHUNK_WAIT", "14"))
NAV_TIMEOUT = 60          # seconds to wait for the score page + challenge
MAX_PAGES = 60            # sanity cap
# Whole-harvest ceiling. Configurable because a weak host (e.g. the NAS N-series
# CPU) renders + screenshots the score noticeably slower than a dev desktop, so
# a desktop-tuned cap can trip mid-harvest. Env lets us tune without a rebuild.
HARVEST_TIMEOUT = int(os.environ.get("HARVEST_TIMEOUT", "360"))
# Per-CDP-call ceiling. Any single Chrome round-trip (evaluate, screenshot,
# emulation) that exceeds this is abandoned so the harvest keeps moving instead
# of wedging until HARVEST_TIMEOUT. Generous enough not to abort a merely-slow
# screenshot on the weak NAS CPU.
CDP_CALL_TIMEOUT = int(os.environ.get("CDP_CALL_TIMEOUT", "20"))


def log(msg: str):
    """Print a flushed line so container logs show harvest progress (Python
    buffers stdout when not a TTY; flush=True + PYTHONUNBUFFERED in the image
    make these visible live)."""
    print(f"[worker] {msg}", flush=True)


# Diagnostic snapshot: scroll container, score <img> inventory (which page
# indices are in the DOM, how many decoded), and the render-liveness probes
# (visibilityState / hasFocus / rAF tick count). `raf` staying flat between
# snapshots means Chrome is not producing frames => IntersectionObserver and
# rAF-driven lazy-load are dead, which is the Xvfb failure mode.
DIAG_JS = r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  const imgs=[...document.querySelectorAll('img')];
  const score=imgs.map(i=>({s:(i.currentSrc||i.src||''),c:i.complete,nw:i.naturalWidth}))
                  .filter(x=>/\/g\/[0-9a-f]+\/score_\d+/i.test(x.s));
  const pages=[...new Set(score.map(x=>(x.s.match(/score_(\d+)/)||[])[1]))].filter(v=>v!=null).sort((a,b)=>a-b);
  return JSON.stringify({
    scEx: !!sc,
    scTop: sc?Math.round(sc.scrollTop):-1,
    scH: sc?sc.scrollHeight:-1,
    winH: window.innerHeight,
    imgTotal: imgs.length,
    scoreImgs: score.length,
    scorePages: pages.join(','),
    scoreLoaded: score.filter(x=>x.c&&x.nw>0).length,
    vis: document.visibilityState,
    foc: document.hasFocus()?1:0,
    raf: window.__raf|0
  });
})()"""

# One-time per-tab setup: a requestAnimationFrame liveness counter, and a
# white background behind the score images (some page PNGs have an alpha
# channel, and page 0 otherwise screenshots with the viewer's gray body
# showing through the transparent pixels).
SETUP_JS = r"""(() => {
  if(!window.__rafInit){window.__rafInit=1;window.__raf=0;
    const f=()=>{window.__raf++;requestAnimationFrame(f)};requestAnimationFrame(f);
    const st=document.createElement('style');
    st.textContent='#jmuse-scroller-component img{background:#fff!important}';
    document.head.appendChild(st);}
  return 1;
})()"""

PAGES_JS = "(window.UGAPP&&UGAPP.store&&UGAPP.store.page&&UGAPP.store.page.data&&UGAPP.store.page.data.score)?UGAPP.store.page.data.score.pages_count:0"

# The main per-score image hash. Primary: the SSR <link rel=preload as=image>
# for page 0 — unambiguous (related-score thumbnails elsewhere on the page have
# their own hashes and would confuse a most-images heuristic). Fallback: the
# hash with the most distinct page indices among large imgs in the scroller.
MAIN_HASH_JS = r"""(() => {
  for (const l of document.querySelectorAll('link[rel="preload"][as="image"], link[as="image"]')) {
    const m=(l.href||'').match(/\/g\/([0-9a-f]+)\/score_\d+/i);
    if(m) return JSON.stringify({mh:m[1],via:'preload'});
  }
  const sc=document.querySelector('#jmuse-scroller-component');
  const bh={};
  (sc||document).querySelectorAll('img').forEach(img=>{
    const s=img.currentSrc||img.src||'';const m=s.match(/\/g\/([0-9a-f]+)\/score_(\d+)/i);
    if(m&&img.getBoundingClientRect().width>300)(bh[m[1]]=bh[m[1]]||{})[m[2]]=1;});
  let best=0,mh='';for(const h in bh){const n=Object.keys(bh[h]).length;if(n>best){best=n;mh=h;}}
  return JSON.stringify({mh,via:'dom',best});
})()"""

# Scroller geometry + a safe in-scroller coordinate to aim wheel events at.
SC_INFO_JS = r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  if(!sc) return JSON.stringify({ex:false});
  const r=sc.getBoundingClientRect();
  return JSON.stringify({ex:true,
    wx:Math.round(r.left+r.width/2),
    wy:Math.round(Math.max(r.top+60, Math.min(r.top+160, window.innerHeight-40))),
    scTop:Math.round(sc.scrollTop), scH:sc.scrollHeight, ch:sc.clientHeight});
})()"""


def scroll_js(pos: int) -> str:
    return (r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  if(!sc) return -1;
  sc.scrollTop=%POS%;
  sc.dispatchEvent(new Event('scroll',{bubbles:true}));
  window.dispatchEvent(new Event('scroll'));
  return Math.round(sc.scrollTop);
})()""").replace('%POS%', str(int(pos)))


def present_js(mh: str) -> str:
    """Score-page <img>s of the main hash that are fully in-viewport, decoded,
    AND settled (fully opaque, nothing translucent painted over them), with
    their viewport rects (screenshot clip coordinates).

    The settled checks matter: the viewer fades each page in over its gray
    body, and a capture that races the fade-in wins deterministically — the
    whole page screenshots ~55% dimmed (uniform gray background). Skipping a
    mid-fade page just defers it to the next poll ~0.45 s later. The
    elementsFromPoint walk likewise skips pages veiled by a translucent
    overlay (modal backdrop, challenge fade-out). Unsettled pages are still
    reported (ok:0) so the caller can capture them anyway as a last resort —
    e.g. if throttled rendering freezes a transition mid-fade."""
    return (r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  const vh=window.innerHeight, out=[];
  const settled=(i,r)=>{
    for(let el=i,d=0;el&&el!==document.body&&d<8;el=el.parentElement,d++){
      if(getComputedStyle(el).opacity!=='1') return false;}
    const cx=Math.max(1,Math.min(window.innerWidth-1,r.left+r.width/2));
    const cy=Math.max(1,Math.min(vh-1,r.top+r.height/2));
    for(const el of document.elementsFromPoint(cx,cy)){
      if(el===i||el.contains(i)) break;
      const cs=getComputedStyle(el), bg=cs.backgroundColor;
      const m=bg.match(/rgba?\(([^)]+)\)/);
      const a=m?(m[1].split(',').length>3?parseFloat(m[1].split(',')[3]):1):0;
      if(a>0.05||(cs.backdropFilter&&cs.backdropFilter!=='none')||cs.backgroundImage!=='none') return false;}
    return true;};
  (sc||document).querySelectorAll('img').forEach(i=>{const s=i.currentSrc||i.src||'';const m=s.match(/\/g\/([0-9a-f]+)\/score_(\d+)/i);if(m&&m[1]==='%MH%'){const r=i.getBoundingClientRect();
    if(r.width>60&&r.height>60&&i.naturalWidth>0&&i.complete&&r.top>=-8&&(r.top+r.height)<=vh+8)
      out.push({n:+m[2],x:r.left,y:r.top,w:r.width,h:r.height,ok:settled(i,r)?1:0});}});
  return JSON.stringify(out);
})()""").replace('%MH%', mh)


def _unwrap(r):
    return r['value'] if isinstance(r, dict) and 'type' in r and 'value' in r else r


async def _ev(tab, expr, ap=False):
    return _unwrap(await tab.evaluate(expr, await_promise=ap))


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


async def harvest_pages(browser, score_id: str):
    """Return a list of per-page PNG bytes for `score_id`, in order.

    Raises RuntimeError with a short reason (plus the last DIAG snapshot) on
    failure (challenge not cleared, no score data, paywalled, etc.).
    """
    log(f"score {score_id}: navigating to score page")
    tab = await browser.get(f"https://musescore.com/score/{score_id}")
    last_diag = {"d": None}

    async def diag(tag):
        try:
            d = await asyncio.wait_for(_ejson(tab, DIAG_JS), timeout=CDP_CALL_TIMEOUT)
            last_diag["d"] = d
            log(f"score {score_id}: DIAG[{tag}] {json.dumps(d)}")
        except Exception as e:
            log(f"score {score_id}: DIAG[{tag}] failed: {e}")

    def fail(reason: str):
        d = f" | diag={json.dumps(last_diag['d'])}" if last_diag["d"] else ""
        raise RuntimeError(reason + d)

    async def cdp_send(what, coro):
        """One CDP round-trip with a hard per-call timeout (a wedged call must
        not freeze the harvest — see CDP_CALL_TIMEOUT). Returns None on miss."""
        try:
            return await asyncio.wait_for(tab.send(coro), timeout=CDP_CALL_TIMEOUT)
        except Exception as e:
            log(f"score {score_id}: {what} failed: {type(e).__name__}: {e}")
            return None

    try:
        pc = await _wait_for(tab, PAGES_JS)
        if not pc:
            fail("no score data (challenge not cleared or not a score)")
        pc = int(pc)
        log(f"score {score_id}: challenge cleared, pages_count={pc}")
        if pc < 1 or pc > MAX_PAGES:
            fail(f"implausible pages_count={pc}")

        # Make the page believe it is the focused, visible, active tab: with no
        # window manager under Xvfb, Chrome may never consider the window
        # focused, and both IntersectionObserver-driven lazy-load and rAF loops
        # can idle in that state.
        await cdp_send("bring_to_front", cdp.page.bring_to_front())
        await cdp_send("focus_emulation", cdp.emulation.set_focus_emulation_enabled(enabled=True))
        try:
            await asyncio.wait_for(_ev(tab, SETUP_JS), timeout=CDP_CALL_TIMEOUT)
        except Exception as e:
            log(f"score {score_id}: setup JS install failed: {e}")

        log(f"score {score_id}: setting emulated viewport {VP_W}x{VP_H}@{VP_SCALE}x")
        await cdp_send("set_device_metrics_override", cdp.emulation.set_device_metrics_override(
            width=VP_W, height=VP_H, device_scale_factor=VP_SCALE, mobile=False))
        await asyncio.sleep(1.0)
        await diag("post-nav")

        # Main score-image hash (preload link is in the SSR head, so this works
        # before any page image is even in the DOM).
        mh = ""
        try:
            r = await asyncio.wait_for(_ejson(tab, MAIN_HASH_JS), timeout=CDP_CALL_TIMEOUT)
            mh = r.get("mh") or ""
            if mh:
                log(f"score {score_id}: main hash {mh[:8]} (via {r.get('via')})")
        except Exception as e:
            log(f"score {score_id}: main-hash probe failed: {e}")

        info = None
        try:
            info = await asyncio.wait_for(_ejson(tab, SC_INFO_JS), timeout=CDP_CALL_TIMEOUT)
        except Exception as e:
            log(f"score {score_id}: scroller probe failed: {e}")
        if not info or not info.get("ex"):
            fail("score scroller (#jmuse-scroller-component) not found")

        # Size the viewport so several pages fit at once: fewer lazy-load
        # triggers needed, and short scores need no scrolling at all.
        sc_h = int(info["scH"])
        page_h = max(300, sc_h // pc)
        vp_h = min(sc_h + 250, VP_H_MAX)
        if vp_h != VP_H:
            log(f"score {score_id}: re-emulating viewport {VP_W}x{vp_h}@{VP_SCALE}x "
                f"(scH={sc_h}, page_h~{page_h})")
            await cdp_send("re-emulate viewport", cdp.emulation.set_device_metrics_override(
                width=VP_W, height=vp_h, device_scale_factor=VP_SCALE, mobile=False))
            await asyncio.sleep(1.2)
            try:
                info = await asyncio.wait_for(_ejson(tab, SC_INFO_JS), timeout=CDP_CALL_TIMEOUT)
                sc_h = int(info["scH"])
            except Exception:
                pass
        wheel_x, wheel_y = int(info.get("wx", VP_W // 2)), int(info.get("wy", 300))

        # Chunk positions: overlap by at least a page height + margin so every
        # page is FULLY visible in at least one chunk (present_js only reports
        # fully-in-viewport images).
        max_top = max(sc_h - vp_h, 0)
        step = max(400, vp_h - page_h - 220)
        positions = list(range(0, max_top + 1, step))
        if positions[-1] != max_top:
            positions.append(max_top)
        log(f"score {score_id}: {len(positions)} chunk(s) at {positions} "
            f"(scH={sc_h}, vp_h={vp_h}, step={step})")

        captured: dict[int, bytes] = {}
        first_unsettled: dict[int, float] = {}

        async def wheel(dy: float):
            await cdp_send("wheel", cdp.input_.dispatch_mouse_event(
                type_="mouseWheel", x=float(wheel_x), y=float(wheel_y),
                delta_x=0.0, delta_y=float(dy)))

        async def goto_pos(pos: int):
            got = -1
            try:
                got = int(await asyncio.wait_for(_ev(tab, scroll_js(pos)), timeout=CDP_CALL_TIMEOUT))
            except Exception as e:
                log(f"score {score_id}: scroll eval failed: {e}")
            if abs(got - pos) > 80:
                # Native scrollTop write didn't take — drive with real wheel
                # input through the browser's input pipeline instead.
                log(f"score {score_id}: scrollTop write ineffective (want {pos}, at {got}); using wheel events")
                for _ in range(14):
                    cur = -1
                    try:
                        i2 = await asyncio.wait_for(_ejson(tab, SC_INFO_JS), timeout=CDP_CALL_TIMEOUT)
                        cur = int(i2.get("scTop", -1))
                    except Exception:
                        pass
                    d = pos - cur
                    if cur < 0 or abs(d) <= 80:
                        break
                    await wheel(max(-1600.0, min(1600.0, float(d))))
                    await asyncio.sleep(0.35)
            # A real wheel jiggle (net zero) regardless: pokes viewers whose
            # lazy-load keys off wheel/scroll input rather than scrollTop.
            await wheel(60.0)
            await wheel(-60.0)

        async def cap() -> int:
            """Screenshot every fully-visible not-yet-captured page. Returns
            how many new pages were captured."""
            nonlocal mh
            if not mh:
                try:
                    r = await asyncio.wait_for(_ejson(tab, MAIN_HASH_JS), timeout=CDP_CALL_TIMEOUT)
                    mh = r.get("mh") or ""
                except Exception:
                    return 0
                if not mh:
                    return 0
            try:
                present = await asyncio.wait_for(_ejson(tab, present_js(mh)), timeout=CDP_CALL_TIMEOUT)
            except Exception:
                present = []
            shots: dict[int, tuple[bytes, dict]] = {}
            for p in present:
                n = p["n"]
                if n in captured:
                    continue
                if not p.get("ok"):
                    # Mid-fade or veiled by a translucent overlay: normally the
                    # next poll gets it crisp. But if it never settles (frozen
                    # transition under throttled rendering, persistent veil),
                    # capture it anyway — a dimmed page beats a missing page.
                    t = first_unsettled.setdefault(n, time.monotonic())
                    if time.monotonic() - t < 6.0:
                        continue
                    log(f"score {score_id}: page {n} never settled; capturing dimmed")
                vp = cdp.page.Viewport(x=p["x"], y=p["y"], width=p["w"], height=p["h"], scale=1.0)
                data = await cdp_send(f"screenshot page {n}", cdp.page.capture_screenshot(
                    format_="png", clip=vp, capture_beyond_viewport=False))
                if data:
                    raw = base64.b64decode(data)
                    if len(raw) > 3000:
                        shots[n] = (raw, p)
            if not shots:
                return 0
            # A layout shift between rect measurement and screenshot (e.g. the
            # video banner expanding above page 0 right at viewer-ready) makes
            # the clip capture the wrong strip. Re-measure: keep only shots
            # whose page rect didn't move; the rest retry on the next poll.
            try:
                chk = await asyncio.wait_for(_ejson(tab, present_js(mh)), timeout=CDP_CALL_TIMEOUT)
                now_at = {q["n"]: q for q in chk}
            except Exception:
                now_at = None
            new = 0
            for n, (raw, p) in shots.items():
                cur = now_at.get(n) if now_at is not None else None
                if now_at is not None and (
                        cur is None or abs(cur["x"] - p["x"]) > 4 or abs(cur["y"] - p["y"]) > 4):
                    log(f"score {score_id}: page {n} moved during capture; will retry")
                    continue
                captured[n] = raw
                new += 1
                log(f"score {score_id}: captured page {n} ({len(raw)} bytes) [{len(captured)}/{pc}]")
            return new

        async def settle_capture():
            """Poll-capture at the current position until all pages are in, the
            chunk budget is spent, or nothing new has appeared for a while."""
            t0 = time.monotonic()
            last_new = t0
            while True:
                if await cap():
                    last_new = time.monotonic()
                now = time.monotonic()
                if len(captured) >= pc:
                    return
                if now - t0 >= CHUNK_WAIT:
                    return
                if now - t0 >= 3.0 and now - last_new >= 3.2:
                    return
                await asyncio.sleep(0.45)

        for pass_i in range(PASSES):
            for pos in positions:
                await goto_pos(pos)
                await settle_capture()
                if len(captured) >= pc:
                    break
            log(f"score {score_id}: pass {pass_i + 1}/{PASSES} done, captured {len(captured)}/{pc}")
            if len(captured) >= pc:
                break
            await diag(f"pass-{pass_i + 1}")

        if len(captured) < pc:
            missing = [n for n in range(pc) if n not in captured]
            fail(f"captured {len(captured)}/{pc} pages; missing {missing}")
        log(f"score {score_id}: all {pc} pages captured")
        return [captured[n] for n in range(pc)]
    finally:
        try:
            await tab.close()
        except Exception:
            pass


def chrome_binary() -> str | None:
    for name in ("google-chrome", "google-chrome-stable", "chrome"):
        p = shutil.which(name)
        if p:
            return p
    return None


def chrome_stderr_probe(args: list[str]) -> str:
    """Launch Chrome directly for a few seconds and capture its stderr.
    nodriver pipes (and never reads) the browser's stderr, so when the launch
    dies the actual crash reason is invisible — this probe recovers it. Only
    used after a failed start; returns a short trimmed transcript."""
    exe = chrome_binary()
    if not exe:
        return "chrome binary not found on PATH"
    probe_args = [exe, *args, "--user-data-dir=/tmp/chrome-probe",
                  "--remote-debugging-port=9223", "about:blank"]
    try:
        p = subprocess.run(probe_args, capture_output=True, text=True, timeout=8)
        out = (p.stderr or "") + (p.stdout or "")
        return f"exit={p.returncode} :: {out.strip()[:1200]}"
    except subprocess.TimeoutExpired as e:
        # Still alive after 8 s = it actually launched fine.
        out = (e.stderr or b"").decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
        return f"probe survived 8s (launch OK) :: {out.strip()[:600]}"
    except Exception as e:
        return f"probe failed to run: {e}"


class Worker:
    def __init__(self):
        self.browser = None
        self.lock = asyncio.Lock()

    def _chrome_args(self) -> list[str]:
        win = os.environ.get("WINDOW", "1500,1000")
        args = [f"--window-size={win}"]
        # Under xvfb-run there is no window manager, so Chrome can consider
        # its window permanently unfocused/occluded and throttle rendering +
        # timers — which kills the score viewer's scroll-triggered lazy-load
        # (IntersectionObserver / rAF never fire). Harmless on a desktop.
        # THROTTLE_FLAGS=off removes them without a rebuild (escape hatch in
        # case a Chrome build chokes on them).
        if os.environ.get("THROTTLE_FLAGS", "on") != "off":
            args += [
                "--disable-background-timer-throttling",
                "--disable-backgrounding-occluded-windows",
                "--disable-renderer-backgrounding",
            ]
        # In Docker we run headful Chrome as root under Xvfb; Chrome refuses
        # the sandbox as root (nodriver also auto-adds --no-sandbox as root).
        # CHROME_EXTRA_ARGS lets the image inject `--no-sandbox` without
        # changing the desktop-tested path (var unset => identical to before).
        args.extend(os.environ.get("CHROME_EXTRA_ARGS", "").split())
        return args

    async def get_browser(self):
        if self.browser is None:
            args = self._chrome_args()
            exe = chrome_binary()
            if exe:
                try:
                    v = subprocess.run([exe, "--version"], capture_output=True,
                                       text=True, timeout=10).stdout.strip()
                    log(f"chrome binary: {exe} ({v})")
                except Exception:
                    pass
            last_err = None
            for attempt in (1, 2, 3):
                log(f"starting Chrome (attempt {attempt}, args={args})")
                try:
                    self.browser = await uc.start(headless=False, browser_args=args)
                    log("Chrome started")
                    return self.browser
                except Exception as e:
                    last_err = e
                    log(f"Chrome start attempt {attempt} failed: {type(e).__name__}: {e}")
                    await asyncio.sleep(2 * attempt)
            # All attempts failed: recover the real crash reason from a direct
            # launch so the 500 body / logs say WHY instead of nodriver's
            # generic "Failed to connect to browser".
            probe = chrome_stderr_probe(args)
            log(f"chrome stderr probe: {probe}")
            raise RuntimeError(
                f"chrome failed to start after 3 attempts: {last_err} | probe: {probe}")
        return self.browser

    def reset_browser(self):
        """Drop the shared browser so the next request starts a fresh one.
        Called after a timeout or CDP/WebSocket error: a cancelled harvest (or a
        dropped `no close frame` connection) leaves Chrome unusable, and every
        later request would otherwise fail instantly on the dead connection.
        `browser.stop()` is synchronous in nodriver."""
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
                return web.json_response({"error": "harvest timed out"}, status=504)
            except RuntimeError as e:
                # Expected "can't get this score" cases -> 422 so the caller can
                # cleanly fall back to linking out. The browser is still healthy
                # here (harvest raised cleanly), so keep it warm.
                log(f"score {score_id}: unharvestable: {e}")
                return web.json_response({"error": str(e)}, status=422)
            except Exception as e:
                # Unexpected (often a dead CDP connection) -> rebuild the browser.
                log(f"score {score_id}: worker error: {e}")
                self.reset_browser()
                return web.json_response({"error": f"worker error: {e}"}, status=500)
        try:
            pdf = img2pdf.convert(pages)
        except Exception as e:
            return web.json_response({"error": f"pdf assembly failed: {e}"}, status=500)
        return web.Response(body=pdf, content_type="application/pdf",
                            headers={"Content-Disposition": f'inline; filename="musescore-{score_id}.pdf"'})


async def healthz(_request):
    return web.Response(text="ok")


def main():
    worker = Worker()
    app = web.Application(client_max_size=64 * 1024 * 1024)
    app.router.add_get("/healthz", healthz)
    app.router.add_get("/pdf/{score_id}", worker.handle_pdf)
    port = int(os.environ.get("PORT", "8000"))
    web.run_app(app, host="0.0.0.0", port=port)


if __name__ == "__main__":
    main()
