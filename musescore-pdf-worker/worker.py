"""
MuseScore PDF worker.

MuseScore serves free community scores only as per-page images behind a
Cloudflare Turnstile challenge and CORS-less cross-origin CDNs, so they cannot
be fetched server-side by a normal HTTP client. This worker drives a real
(nodriver) Chrome that passes the challenge, scrolls the score viewer, and
screenshots each rendered page at ~200 DPI, then stitches the pages into a PDF.

HTTP API (FlareSolverr-independent, dead simple):
    GET /healthz         -> "ok"
    GET /pdf/<score_id>  -> application/pdf  (or 4xx/5xx + JSON error)

Env:
    PORT   (default 8000)
    WINDOW (default "1500,1000")   host window size for headful Chrome
    VP_W, VP_H, VP_SCALE           emulated viewport (default 1400x2200 @2x)

Design notes:
  * One shared browser; requests are serialized with an asyncio.Lock because a
    single Chrome can only sensibly render one score at a time (and it keeps the
    memory footprint bounded). Each request uses its own tab, closed at the end.
  * Everything Cloudflare-sensitive happens inside the browser; the only bytes
    that leave are the finished page PNGs / PDF.
"""

import asyncio
import base64
import json
import os
import nodriver as uc
from nodriver import cdp
from aiohttp import web
import img2pdf

VP_W = int(os.environ.get("VP_W", "1400"))
VP_H = int(os.environ.get("VP_H", "2200"))
VP_SCALE = int(os.environ.get("VP_SCALE", "2"))
NAV_TIMEOUT = 60          # seconds to wait for the score page + challenge
MAX_PAGES = 60            # sanity cap

STEP_JS = r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  const bh=(window.__bh=window.__bh||{});
  (sc||document).querySelectorAll('img').forEach(img=>{const s=img.currentSrc||img.src||'';const m=s.match(/\/g\/([0-9a-f]+)\/score_(\d+)\./i);if(m)(bh[m[1]]=bh[m[1]]||{})[m[2]]=1;});
  if(sc){sc.scrollTop=Math.min(sc.scrollTop+380,sc.scrollHeight);}
  window.scrollBy(0,300);
  let best=0,mh='';for(const h in bh){const n=Object.keys(bh[h]).length;if(n>best){best=n;mh=h;}}
  return JSON.stringify({best, mh});
})()"""

PAGES_JS = "(window.UGAPP&&UGAPP.store&&UGAPP.store.page&&UGAPP.store.page.data&&UGAPP.store.page.data.score)?UGAPP.store.page.data.score.pages_count:0"


def present_js(mh: str) -> str:
    return (r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  const vh=window.innerHeight, out=[];
  (sc||document).querySelectorAll('img').forEach(i=>{const s=i.currentSrc||i.src||'';const m=s.match(/\/g\/([0-9a-f]+)\/score_(\d+)\./i);if(m&&m[1]==='%MH%'){const r=i.getBoundingClientRect();
    if(r.width>60&&r.height>60&&i.naturalWidth>0&&i.complete&&r.top>=-8&&(r.top+r.height)<=vh+8)
      out.push({n:+m[2],x:r.left,y:r.top,w:r.width,h:r.height});}});
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

    Raises RuntimeError with a short reason on failure (challenge not cleared,
    no score data, paywalled, etc.).
    """
    tab = await browser.get(f"https://musescore.com/score/{score_id}")
    try:
        pc = await _wait_for(tab, PAGES_JS)
        if not pc:
            raise RuntimeError("no score data (challenge not cleared or not a score)")
        pc = int(pc)
        if pc < 1 or pc > MAX_PAGES:
            raise RuntimeError(f"implausible pages_count={pc}")

        await tab.send(cdp.emulation.set_device_metrics_override(
            width=VP_W, height=VP_H, device_scale_factor=VP_SCALE, mobile=False))
        await asyncio.sleep(1.0)

        captured: dict[int, bytes] = {}
        mh = ""
        # warm-up: one scroll so every page loads once and the main hash is known
        for _ in range(60):
            try:
                r = await _ejson(tab, STEP_JS)
                mh = r.get("mh") or mh
                if r.get("best", 0) >= pc:
                    break
            except Exception:
                pass
            await asyncio.sleep(0.3)
        if not mh:
            raise RuntimeError("no page images found")

        async def cap():
            try:
                present = await _ejson(tab, present_js(mh))
            except Exception:
                present = []
            for p in present:
                n = p["n"]
                if n in captured:
                    continue
                vp = cdp.page.Viewport(x=p["x"], y=p["y"], width=p["w"], height=p["h"], scale=1.0)
                try:
                    data = await tab.send(cdp.page.capture_screenshot(
                        format_="png", clip=vp, capture_beyond_viewport=False))
                    raw = base64.b64decode(data)
                    if len(raw) > 3000:
                        captured[n] = raw
                except Exception:
                    pass

        for _pass in range(6):
            if len(captured) >= pc:
                break
            await _ev(tab, "(()=>{const sc=document.querySelector('#jmuse-scroller-component');if(sc)sc.scrollTop=0;window.scrollTo(0,0);return 1;})()")
            await asyncio.sleep(0.6)
            for _ in range(90):
                await cap()
                if len(captured) >= pc:
                    break
                await _ev(tab, "(()=>{const sc=document.querySelector('#jmuse-scroller-component');if(sc)sc.scrollTop+=240;window.scrollBy(0,180);return 1;})()")
                await asyncio.sleep(0.32)
                await cap()

        if len(captured) < pc:
            missing = [n for n in range(pc) if n not in captured]
            raise RuntimeError(f"captured {len(captured)}/{pc} pages; missing {missing}")
        return [captured[n] for n in range(pc)]
    finally:
        try:
            await tab.close()
        except Exception:
            pass


class Worker:
    def __init__(self):
        self.browser = None
        self.lock = asyncio.Lock()

    async def get_browser(self):
        if self.browser is None:
            win = os.environ.get("WINDOW", "1500,1000")
            self.browser = await uc.start(headless=False, browser_args=[f"--window-size={win}"])
        return self.browser

    async def handle_pdf(self, request: web.Request) -> web.Response:
        score_id = request.match_info["score_id"]
        if not score_id.isdigit():
            return web.json_response({"error": "score_id must be numeric"}, status=400)
        async with self.lock:
            try:
                browser = await self.get_browser()
                pages = await asyncio.wait_for(harvest_pages(browser, score_id), timeout=240)
            except asyncio.TimeoutError:
                return web.json_response({"error": "harvest timed out"}, status=504)
            except RuntimeError as e:
                # Expected "can't get this score" cases -> 422 so the caller can
                # cleanly fall back to linking out.
                return web.json_response({"error": str(e)}, status=422)
            except Exception as e:
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
