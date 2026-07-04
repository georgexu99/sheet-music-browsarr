# MuseScore PDF worker — handoff / resume doc

**Status (2026-07-04): core proven & committed. Remaining work = deployment.**
This doc is self-contained so you can `/clear` and resume from here.

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

### 5a. Dockerfile (the one real remaining risk)
Package `worker.py` in a container with Python + Google Chrome + nodriver + Xvfb
(headful Chrome under a virtual display). **Risk to verify first:** does nodriver
clear the Turnstile from *inside Docker/Xvfb*? Desktop headful works; the old
`21hsmw/flaresolverr:nodriver` image (nodriver in Docker) DID pass, so base the
Dockerfile on a similar pattern (or start `FROM` a maintained
python+chrome+xvfb image). Entrypoint: `xvfb-run -a python worker.py` (or start
Xvfb + `DISPLAY=:99`). Expose `PORT` (suggest **8194** — 8191/8192/8193 taken).
Give it `shm_size: 2gb`. Deploy as a new Portainer stack `musescore-pdf-worker`,
publish `8194:8194`. Smoke test: `curl http://10.0.0.91:8194/pdf/5739597 -o t.pdf`
should be `%PDF` and ~2 MB.

### 5b. Wire Rust `fetch_pdf_bytes`
In `src/sources/musescore.rs`, replace the jmuse/bundle/salt PDF pipeline for the
download path with a call to the worker: `GET {WORKER_URL}/pdf/{id}` (new env
`MUSESCORE_PDF_WORKER_URL`, e.g. `http://10.0.0.91:8194`), return the PDF bytes
directly (respect `max_bytes`). On worker 422/5xx or unset URL → keep the current
behavior (fall back to `external_url` link-out). The `pdf_handler` in
`src/routes/public.rs` already 302s to `external_url` on `Err`, so returning an
error preserves the link-out. Keep the boa/jmuse code or delete it — it no longer
works against current MuseScore (search page dropped the `data-hex` hydration).

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

### 5d. Speed
~160 s/score now (Chrome cold-start + conservative 6-pass harvest). Tune: reuse a
warm browser+tab, fewer passes, larger steps, stop as soon as `pages_count`
captured. Target < 60 s. Consider a disk PDF cache keyed by score id (the app has
a cache-dir pattern already; see `docs/plan.md` Phase F).

### 5e. Cleanup
Once the worker is the MuseScore path: tear down the `byparr` (8192) and
`flaresolverr-nodriver` (8193) test stacks (unless keeping nodriver for the search
HTML). Repoint stack 7 off `FLARESOLVERR_URL` if no longer used by MuseScore
(note: UltimateGuitar also reads it — check before removing).

---

## 6. Quick resume checklist
- [ ] Write `musescore-pdf-worker/Dockerfile`; build/deploy stack on **8194**;
      `curl .../pdf/5739597` returns a real PDF (proves Xvfb challenge-pass).
- [ ] Rust: `fetch_pdf_bytes` → worker; env `MUSESCORE_PDF_WORKER_URL`; 422→link-out.
- [ ] Deploy app; click "Open PDF" on a free MuseScore result → real PDF.
- [ ] Fix search parser (5c); un-skip MuseScore in search.
- [ ] Optimize speed; add PDF cache.
- [ ] Tear down test stacks.
