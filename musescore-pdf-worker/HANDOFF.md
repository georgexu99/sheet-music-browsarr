# MuseScore PDF worker — handoff / resume doc

**Status (2026-07-05): DEPLOYED to the NAS. Container is healthy and — the big
one — Chrome-under-Xvfb-in-Docker CLEARS the Cloudflare Turnstile. One blocker
left: the screenshot-capture phase hangs in-container (works on desktop).**
This doc is self-contained so you can `/clear` and resume from here.

## ⭐ 2026-07-05 deployment session — READ THIS FIRST

**Worker is live on the NAS:** Portainer stack `musescore-pdf-worker` (stack
**id=13**, endpoint 3), publishes **8194**, pulls
`ghcr.io/georgexu99/musescore-pdf-worker:latest` (a **public** GHCR package). CI
(`.github/workflows/musescore-pdf-worker.yml`) builds+pushes it on every push to
`main`. Branch merged to main; main HEAD ≈ `4bc02d0`.

**✅ CORE RISK RETIRED:** container logs on a real `/pdf/5739597` request show:
```
[worker] starting Chrome
[worker] Chrome started
[worker] score 5739597: navigating to score page
[worker] score 5739597: challenge cleared, pages_count=7
```
So **Xvfb + headful Chrome in Docker passes the Turnstile** and reads the score.
This was the whole project's open question. Answered: yes.

**🚧 BLOCKER (next task): the harvest hangs right after clearing the challenge.**
The log stops at `challenge cleared` and never reaches
`main hash … found; starting capture passes`. So it hangs in the ~3 lines
between: `cdp.emulation.set_device_metrics_override(...)` (the 1400×2200@2x
emulated viewport) **or** the warm-up `STEP_JS` `tab.evaluate` loop. On desktop
these return fine; under Xvfb one of them blocks, the outer `HARVEST_TIMEOUT`
(420 s on the NAS) eventually fires → 504, and `reset_browser()` recycles Chrome.
**Hypotheses (most→least likely):** (1) `set_device_metrics_override` with
`device_scale_factor=2` hangs Chrome's compositor on the virtual display — try
`device_scale_factor=1` (bump VP_W/VP_H to compensate) or drop the emulation and
size via `--window-size`/`--force-device-scale-factor`; (2) a `tab.evaluate`
CDP call hangs — wrap every `tab.send`/`tab.evaluate` in a short
`asyncio.wait_for` so one stuck call can't freeze the whole harvest; (3) GPU/
compositor: add `--disable-gpu --disable-software-rasterizer --use-gl=swiftshader`
to `CHROME_EXTRA_ARGS`. **Iterate:** edit worker.py → push main → wait worker CI
→ Portainer stack 13 "Redeploy from git repository / Pull and redeploy" (Update)
→ `curl -m 480 http://10.0.0.91:8194/pdf/5739597` → read stack-13 container logs.
The `log()` milestones + `PYTHONUNBUFFERED=1` make progress visible live.

**Deploy gotchas already solved (don't rediscover):**
- **`xauth`** is a *runtime* dep of `xvfb-run` and a *separate* Debian package
  from `xvfb`. Missing → `xvfb-run: error: xauth command not found` → crash-loop.
  A successful image *build* does NOT catch it. It's in the Dockerfile now.
- **`pull_policy: always`** in the compose is mandatory: the image republishes
  under `:latest`, and Portainer's "re-pull image" checkbox alone kept the stale
  cached digest — the container ran the pre-fix image across two redeploys.
- **GHCR tag propagation:** after a push, wait ~1 min before redeploying or the
  NAS may pull the previous `:latest`.
- **App auto-deploy webhook is BROKEN (404):** `release.yml`'s "Trigger Portainer
  redeploy" step fails with `curl (22) 404` — the `PORTAINER_WEBHOOK_URL` secret
  is stale/deleted. The app image (with the worker-wiring code) IS on GHCR
  `:latest`, but the **live app was not redeployed** and still runs pre-worker
  code. Fix the webhook (or redeploy stack 7 manually) as separate cleanup.

**To fully activate "Open PDF → real PDF" once the harvest is fixed:**
1. Redeploy app stack **7** (`sheet-music-browsarr`) to pull the new app image
   (webhook is 404, so do it manually in Portainer / fix the webhook).
2. Add env `MUSESCORE_PDF_WORKER_URL=http://10.0.0.91:8194` to stack 7, redeploy.
3. Note the slow harvest vs the Rust client timeout (`PDF_WORKER_TIMEOUT=300 s`
   in `musescore.rs` vs `HARVEST_TIMEOUT=420 s`): the client will abort first on
   slow scores. Route slow/large scores through the async "Email me this PDF"
   flow (see §5c-note / §5d) rather than blocking the click.

**Progress from the earlier (2026-07-04) session:**
- **5a Dockerfile — WRITTEN + re-verified.** `Dockerfile`, `docker-compose.yml`
  (pulls the GHCR image), and CI (`.github/workflows/musescore-pdf-worker.yml`,
  builds→`ghcr.io/georgexu99/musescore-pdf-worker:latest`) are done. `worker.py`
  gained a `CHROME_EXTRA_ARGS` hook so the image can inject
  `--no-sandbox --disable-dev-shm-usage` (root Chrome under Xvfb) without
  touching the desktop path. Re-ran `worker.py` on the desktop → `/pdf/5739597`
  = **HTTP 200, 2,050,312 bytes, 7-page %PDF**. ✅
  **STILL UNVERIFIED: does Xvfb-in-Docker clear the Turnstile?** Local Docker is
  dead on the dev box (no WSL2 → Docker Desktop won't start), so the container
  couldn't be built/run here. This gets verified on the NAS at deploy time
  (smoke test below). The desktop egress == the NAS egress (same residential
  WAN), so the only untested variable is the Xvfb layer, not the IP.
- **5b Rust wiring — DONE.** `src/sources/musescore.rs`: new
  `MUSESCORE_PDF_WORKER_URL` env + `fetch_pdf_via_worker()`; `fetch_pdf_bytes`
  delegates to the worker when the env is set, streams the PDF under `max_bytes`,
  and returns `Err` on 4xx/5xx/timeout/non-PDF → `pdf_handler` 302s to the score
  page (link-out preserved). Legacy jmuse/bundle/salt pipeline kept as the
  unset-env fallback. `cargo check` clean; 7 musescore unit tests pass.

---

## 1. TL;DR

MuseScore free scores can't be downloaded server-side by any normal HTTP client
(Cloudflare Turnstile + CORS-less cross-origin image CDNs). The **only** thing
that works is driving a real headless Chrome that passes the challenge and
**screenshots each rendered page**. That is built and verified:

- `musescore-pdf-worker/worker.py` — aiohttp service, `GET /pdf/<score_id>` →
  `application/pdf`. Verified locally: score `5739597` → valid **2 MB, 7-page
  PDF** of clean ~200 DPI sheet music.
- `musescore-pdf-worker/prototype_harvest.py` — standalone proof-of-concept
  (saves per-page PNGs to `out/<id>/`). Run: `py prototype_harvest.py 5739597`.

**Remaining (ordered): Dockerfile → deploy worker → wire Rust `fetch_pdf_bytes`
→ fix MuseScore search parser → optimize speed → tear down test stacks.**

---

## 2. Why (what we ruled out — don't re-try these)

- reqwest direct fetch → Cloudflare `403 "Just a moment"` (TLS-fingerprint bot block).
- FlareSolverr 3.3.21 → **cannot solve** the Turnstile (90 s timeout; fails on a
  control CF site too).
- Byparr (Camoufox) → passes/fast but returns the interstitial, never the page.
- nodriver/Chrome (`21hsmw/flaresolverr:nodriver`) → **DOES** clear the challenge,
  but returns the shell/URLs only.
- `cf_clearance` is **Chrome-JA3-bound** → replaying it from reqwest/Node = 403.
- `/api/jmuse` and the page-image CDNs (`cdn.ustatik.com`, `s3w.musescore.com`)
  are **fingerprint-walled** → non-Chrome 403; in-page `fetch()` = CORS
  "Failed to fetch"; `crossOrigin` img load fails (no CORS headers); canvas taints.
- => the images can only be read as **rendered pixels** (screenshots) inside the
  passing Chrome. That's what the worker does.

Full blow-by-blow is in Claude memory: `musescore-cloudflare-blocked.md`.

## 3. The winning harvest technique (already in worker.py — reference only)

1. `nodriver` **headful** Chrome (headless is more detectable; in Docker use Xvfb).
2. Navigate `https://musescore.com/score/<id>`; wait for
   `window.UGAPP.store.page.data.score.pages_count` (challenge auto-clears ~9 s).
3. Tall 2× **emulated viewport**:
   `cdp.emulation.set_device_metrics_override(width=1400,height=2200,device_scale_factor=2)`.
4. Python-driven scroll to discover the **main per-score image hash** + page count
   (image URLs look like `…/scoredata/g/<PER-PAGE-hash>/score_<N>.<png|svg>`).
5. **Multiple** top→bottom passes, small (~240 px) steps. Each step, screenshot any
   not-yet-captured page whose `<img>` (matching main hash) is **fully in-viewport
   AND loaded** (`naturalWidth>0 && complete`) via
   `cdp.page.capture_screenshot(clip=Viewport(x,y,w,h,scale=1), capture_beyond_viewport=False)`.
   - viewport coords + `beyond_viewport=False` is critical (beyondViewport anchors
     at doc-top → captures the site header instead of the page).
   - multi-pass because the viewer **virtualizes** (unloads off-screen pages);
     missed pages are cached on the next pass.
6. `img2pdf.convert([png bytes…])` → PDF.

**nodriver gotchas:** `tab.evaluate()` returns CDP-wrapped `{type,value}` — unwrap
it (helper `_unwrap`); prefer returning `JSON.stringify(...)` and `json.loads`.
`browser.stop()` is sync (no `await`). `cdp.network.get_response_body()` is broken
in 0.50.3 (kills the WS) — that's why we screenshot instead of capturing bytes.

**Test scores:** `5739597` (free, 7 pages ✅). Official/paid scores have
`is_pdf:true` and only expose a preview — worker returns 422; caller must fall
back to link-out. `harvest_pages()` already raises on can't-harvest.

---

## 4. Current production state (DO NOT lose track)

Portainer (`portainer.xuhome.casa`, endpoint id **3**, v2.33.6, CSRF via
`X-CSRF-Token` header; stack API worked via the browser session, see git history):

- **sheet-music-browsarr** = stack **id 7**. Env now:
  `FLARESOLVERR_URL=http://10.0.0.91:8193`, `FLARESOLVERR_POOL_SIZE=1`
  (+ BROWSARR_SECRET_KEY, BROWSARR_ADMIN_PASSWORD). MuseScore is `SourceHealth`-
  degraded → **skipped**, so search is fast (~0.7 s) with IMSLP+Mutopia; MuseScore
  returns nothing but doesn't hang. Site healthy.
- Solver test stacks running on the NAS (host `10.0.0.91`):
  `8191` = FlareSolverr (SHARED with Prowlarr — leave alone),
  `8192` = **byparr** stack, `8193` = **flaresolverr-nodriver** stack.
- Code already merged to `main` / on branch `claude/adoring-swartz-4b72cf`:
  dl-librescore-method port + direct-first (`src/sources/musescore.rs`), and the
  worker (`musescore-pdf-worker/`, commit `f85d610`).

The app's egress = the home residential WAN IP (same as the dev box), which is
what lets Chrome pass the challenge. The NAS is a TerraMaster F4-424 (see
`nas-context`).

---

## 5. Next steps

### 5a. Dockerfile — DONE (build/run verification pending on NAS)
Files written: `Dockerfile` (python:3.12-slim + google-chrome-stable + Xvfb +
fonts + dumb-init; entrypoint `dumb-init -- xvfb-run -a -s "-screen 0
1920x2400x24" python worker.py`; `EXPOSE 8194`), `docker-compose.yml` (Portainer
stack, pulls the GHCR image, `shm_size: 2gb`, mem-limit 2 g, healthcheck), and CI
`.github/workflows/musescore-pdf-worker.yml`. **The one remaining risk is still
open:** does Xvfb-in-Docker clear the Turnstile? Couldn't build locally (no WSL2).
**Verify at deploy:** on the NAS, `curl http://10.0.0.91:8194/pdf/5739597 -o t.pdf`
must be `%PDF` and ~2 MB. If it 422s / times out, the Xvfb Chrome isn't passing —
compare with the proven `21hsmw/flaresolverr:nodriver` flags before deep-diving.

**Deploy path (mirrors the app):** push branch→main → CI builds+pushes
`ghcr.io/georgexu99/musescore-pdf-worker:latest` → create Portainer stack
`musescore-pdf-worker` from `musescore-pdf-worker/docker-compose.yml` (publishes
`8194:8194`). No build context needed on the NAS — the stack just pulls the image.

### 5b. Wire Rust `fetch_pdf_bytes` — DONE
`src/sources/musescore.rs`: `PDF_WORKER_ENV = "MUSESCORE_PDF_WORKER_URL"`, new
`fetch_pdf_via_worker()` (300 s per-request timeout override, streams the PDF
under `max_bytes`, `%PDF-` sniff, non-2xx → `Err`). `fetch_pdf_bytes` early-
returns through the worker when the env is set; otherwise falls through to the
legacy jmuse pipeline (kept, not deleted — harmless, and it's the no-worker
fallback). `pdf_handler` already 302s to `external_url` on `Err`, so 422/5xx/
timeout all preserve the link-out. **To activate: set
`MUSESCORE_PDF_WORKER_URL=http://10.0.0.91:8194` on app stack 7 + redeploy.**

### 5c. MuseScore search parser (separate from PDF)
Current search is degraded/skipped. MuseScore dropped the `data-<hex>` SSR
hydration JSON that `find_hydration_json` reads; results are now plain
`<a href="/scores/N">` anchors (~37 on the rendered page) + a client-side
`window.UGAPP.store.page.data.scores`. The existing `extract_search_scores_from_dom`
DOM fallback should work **but only on the fully rendered page** — which means
search also needs the nodriver solver's rendered HTML (the solver returns the
full 465 KB page with 40 score links when its session is warm). Options: (i) add
a `/search?q=` endpoint to the worker that returns the rendered results HTML/JSON,
or (ii) get the search HTML from the nodriver solver (8193) and lean on the DOM
fallback. Lower priority than the PDF path.

### 5c-note. Search vs PDF are already decoupled (important UX point)
`Source::search` returns metadata ONLY (title, composer, thumbnail, "N pages"
badge, difficulty). `fetch_pdf_bytes` (the ~160 s worker) runs ONLY on the
"Open PDF" click (`/pdf/musescore/{id}`), and also `/email` + `/admin/library/add`.
So the worker slowness CANNOT slow search — result cards render instantly once
5c lands. The 160 s is purely the click-to-download wait. Make that wait pleasant:
speed (5d) + PDF cache + a progress/spinner page instead of a blocking request +
route slow scores through the existing **"Email me this PDF"** flow (async
generate → email; user never blocks). Don't pre-generate PDFs at search time.

### 5d. Speed + concurrency (known limitation)
~160 s/score now (Chrome cold-start + conservative 6-pass harvest). Tune: reuse a
warm browser+tab, fewer passes, larger steps, stop as soon as `pages_count`
captured. Target < 60 s. Consider a disk PDF cache keyed by score id (the app has
a cache-dir pattern already; see `docs/plan.md` Phase F).

**Concurrency ceiling (flagged in adversarial review):** the worker serializes
every request behind one `asyncio.Lock` held for the whole ~160–240 s harvest
(one shared Chrome). The Rust client's 300 s timeout starts when the request is
*sent*, so a second `/pdf` queued behind a long harvest can burn most of its
budget just waiting and then time out → link-out, even though it was never
processed. Throughput is ~1 PDF / ~4 min; the admin "add to library" path and a
concurrent public click can collide. Degrades to a graceful link-out (not a
crash), but it's why bursts will show intermittent MuseScore failures. Real fix
lands with 5d/5c-note: a PDF cache (dedupes repeat clicks) + routing slow/queued
scores through the async **"Email me this PDF"** flow so users never block on the
lock. A warm-tab pool (N Chromes) would raise the ceiling if needed.

### 5e. Cleanup
Once the worker is the MuseScore path: tear down the `byparr` (8192) and
`flaresolverr-nodriver` (8193) test stacks (unless keeping nodriver for the search
HTML). Repoint stack 7 off `FLARESOLVERR_URL` if no longer used by MuseScore
(note: UltimateGuitar also reads it — check before removing).

---

## 6. Quick resume checklist
- [x] Write `musescore-pdf-worker/Dockerfile` (+ compose + CI). worker.py
      `CHROME_EXTRA_ARGS` hook added; desktop re-verified (2 MB 7-page PDF).
- [x] Rust: `fetch_pdf_bytes` → worker; env `MUSESCORE_PDF_WORKER_URL`;
      422/5xx/timeout → link-out. `cargo check` + unit tests pass.
- [x] Merge to main; CI publishes `ghcr.io/georgexu99/musescore-pdf-worker:latest`.
- [x] **Deploy worker stack on 8194** (Portainer stack id=13, pulls GHCR image).
      Container healthy; `/healthz` = ok.
- [x] **Proved Xvfb-in-Docker clears the Turnstile** (`challenge cleared,
      pages_count=7` in the container logs). ⭐ core risk retired.
- [ ] **FIX THE CAPTURE HANG** (top of doc): harvest stalls right after the
      challenge clears (emulation/`set_device_metrics_override` or STEP_JS under
      Xvfb). Then `curl .../pdf/5739597` should return a real ~2 MB %PDF.
- [ ] Fix the app auto-deploy webhook (404) OR redeploy stack 7 manually so the
      live app runs the worker-wiring code.
- [ ] Set `MUSESCORE_PDF_WORKER_URL=http://10.0.0.91:8194` on app stack 7,
      redeploy; click "Open PDF" on a free MuseScore result → real PDF.
- [ ] Fix search parser (5c); un-skip MuseScore in search.
- [ ] Optimize speed; add PDF cache; route slow scores through the email flow.
- [ ] Tear down test stacks (byparr 8192 / flaresolverr-nodriver 8193).
