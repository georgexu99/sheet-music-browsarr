import asyncio, base64, json, os, sys, re
import nodriver as uc
from nodriver import cdp

def unwrap(r):
    return r['value'] if isinstance(r, dict) and 'type' in r and 'value' in r else r
async def ev(tab, expr, ap=False):
    return unwrap(await tab.evaluate(expr, await_promise=ap))
async def ejson(tab, expr, ap=False):
    s = await ev(tab, expr, ap); return json.loads(s) if isinstance(s, str) else s
async def wait_for(tab, expr, tries=45):
    for _ in range(tries):
        try:
            r = await ev(tab, expr)
            if r: return r
        except Exception: pass
        await asyncio.sleep(1)
    return None

# small scroll step; also reports the current main hash (hash with most pages seen so far)
STEP_JS = r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  const bh=(window.__bh=window.__bh||{});
  (sc||document).querySelectorAll('img').forEach(img=>{const s=img.currentSrc||img.src||'';const m=s.match(/\/g\/([0-9a-f]+)\/score_(\d+)\./i);if(m)(bh[m[1]]=bh[m[1]]||{})[m[2]]=1;});
  if(sc){sc.scrollTop=Math.min(sc.scrollTop+380,sc.scrollHeight);}
  window.scrollBy(0,300);
  let best=0,mh='';for(const h in bh){const n=Object.keys(bh[h]).length;if(n>best){best=n;mh=h;}}
  return JSON.stringify({best, mh});
})()"""

def present_js(mh):
    return (r"""(() => {
  const sc=document.querySelector('#jmuse-scroller-component');
  const vh=window.innerHeight, out=[];
  (sc||document).querySelectorAll('img').forEach(i=>{const s=i.currentSrc||i.src||'';const m=s.match(/\/g\/([0-9a-f]+)\/score_(\d+)\./i);if(m&&m[1]==='%MH%'){const r=i.getBoundingClientRect();
    if(r.width>60&&r.height>60&&i.naturalWidth>0&&i.complete&&r.top>=-8&&(r.top+r.height)<=vh+8)
      out.push({n:+m[2],x:r.left,y:r.top,w:r.width,h:r.height});}});
  return JSON.stringify(out);
})()""").replace('%MH%', mh)

async def main():
    sid = sys.argv[1] if len(sys.argv) > 1 else '5739597'
    browser = await uc.start(headless=False, browser_args=['--window-size=1500,1000'])
    tab = await browser.get(f'https://musescore.com/score/{sid}')
    pc = await wait_for(tab, "(window.UGAPP&&UGAPP.store&&UGAPP.store.page&&UGAPP.store.page.data&&UGAPP.store.page.data.score)?UGAPP.store.page.data.score.pages_count:0")
    print('pages_count', pc, flush=True)
    if not pc:
        try: browser.stop()
        except: pass
        return
    pc = int(pc)
    # tall high-DPI emulated viewport so a full page fits and virtualization keeps it rendered
    await tab.send(cdp.emulation.set_device_metrics_override(width=1400, height=2200, device_scale_factor=2, mobile=False))
    await asyncio.sleep(1.0)

    os.makedirs(f'out/{sid}', exist_ok=True)
    captured = {}
    mh = ''
    # warm-up: one full scroll so every page loads once (populates main hash + cache)
    for i in range(60):
        try:
            r = await ejson(tab, STEP_JS)
            mh = r.get('mh') or mh
            if r.get('best', 0) >= pc: break
        except Exception: pass
        await asyncio.sleep(0.3)

    async def cap_present():
        try:
            present = await ejson(tab, present_js(mh))
        except Exception:
            present = []
        for p in present:
            n = p['n']
            if n in captured: continue
            vp = cdp.page.Viewport(x=p['x'], y=p['y'], width=p['w'], height=p['h'], scale=1.0)
            try:
                data = await tab.send(cdp.page.capture_screenshot(format_='png', clip=vp, capture_beyond_viewport=False))
                raw = base64.b64decode(data)
                if len(raw) > 3000:
                    open(f"out/{sid}/page_{n:02d}.png", 'wb').write(raw)
                    captured[n] = f"{len(raw)}b:{int(p['w']*2)}x{int(p['h']*2)}"
            except Exception:
                pass

    # multiple top->bottom passes with small steps; missed pages get caught once cached
    for pass_i in range(5):
        if len(captured) >= pc: break
        await ev(tab, "(()=>{const sc=document.querySelector('#jmuse-scroller-component');if(sc)sc.scrollTop=0;window.scrollTo(0,0);return 1;})()")
        await asyncio.sleep(0.6)
        for i in range(80):
            await cap_present()
            if len(captured) >= pc: break
            await ev(tab, "(()=>{const sc=document.querySelector('#jmuse-scroller-component');if(sc)sc.scrollTop+=240;window.scrollBy(0,180);return 1;})()")
            await asyncio.sleep(0.32)
            await cap_present()
    saved = [f"{n}:{captured.get(n,'MISS')}" for n in range(pc)]
    ok = len(captured)
    print('RESULT', json.dumps({'pages': pc, 'okPages': ok, 'mh': mh[:8], 'detail': saved}), flush=True)
    print('FULL_SUCCESS', ok == pc and ok > 0, flush=True)
    try: browser.stop()
    except: pass

asyncio.run(main())
