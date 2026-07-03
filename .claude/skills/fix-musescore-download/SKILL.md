---
name: fix-musescore-download
description: >-
  Diagnose and fix MuseScore PDF downloads when they break. Use when MuseScore
  scores "only return the preview", redirect to the musescore.com link instead
  of serving a PDF, or when logs show `fetch_pdf_bytes failed; falling back to
  external_url`. Covers the per-page `/api/jmuse` auth-token algorithm, the
  bundle salt extraction, and the search-page scraper — the parts that break
  when MuseScore ships a site change.
---

# Fix MuseScore downloads

MuseScore.com serves community uploads only as **per-page PNGs** behind an
authenticated API (`/api/jmuse`), not as a server-side PDF. We fetch each page
PNG and stitch a PDF (`printpdf`). When this breaks, the app falls back to
redirecting the user to `musescore.com/score/{id}`, which only shows the
on-site **preview** — that is the symptom users report.

All the code lives in `src/sources/musescore.rs`. The HTTP handler that falls
back to the preview link is `pdf_handler` in `src/routes/public.rs`.

## The pipeline (and where each step can break)

`fetch_pdf_bytes(id)` does, in order:

1. **`fetch_score_page`** → `fetch_html_challenged`. **Direct-first**
   (dl-librescore's method): `try_direct` does a plain browser-style GET; only
   if that comes back as a Cloudflare challenge (or non-2xx) does it fall back
   to FlareSolverr, and only when `FLARESOLVERR_URL` is set. From a residential
   egress (e.g. the NAS's home IP) the direct GET usually succeeds and FS is
   never touched — so unsetting `FLARESOLVERR_URL` runs pure-direct. Breaks only
   if the egress *is* Cloudflare-challenged AND there's no FS fallback (error:
   `direct fetch did not return the page …; no FlareSolverr configured`) — the
   fix there is a residential/HTTP proxy on the egress. Also lifts, best-effort,
   the candidate bundle URLs, the page count, and the **static page-0 image
   URL** (`<link as="image">`, `@`-suffix stripped) — none fatal on its own.
2. **`extract_bundle_urls`** — collect *every* `…/build/musescore…/20….js` URL
   (`is_bundle_candidate`), not just one. Empty is non-fatal (fallback salt covers it).
3. **`extract_pages_count`** — hydration JSON (`extract_score_meta`), then a
   `pages":<n>` regex fallback. Unknown ⇒ assume 1 with a loud `warn!` (a
   multi-page score would otherwise truncate silently).
4. **`prepare_algorithm`** → `find_random_token` — try each candidate bundle,
   return the salt from the first that has the `"…").substr(0,4)` literal.
   Now returns `Option` (no salt is non-fatal — the fallback takes over).
5. **`mint_token`** per page — compute the token **natively** (MD5). See below.
6. **`resolve_page_url`** → **`jmuse_url`** per page (page 0 uses the static URL,
   no token) → CDN PNG fetch → **`assemble_pdf`**. Each page tries the extracted
   salt then the **hardcoded fallback salt** before failing; the winning salt is
   reused for later pages. `musescore jmuse error: …` on *both* salts means the
   score is MuseScore-Pro-only content (expected for some — try another), or the
   fallback salt has gone stale (see below).

**All direct calls (bundle JS, `/api/jmuse`, CDN PNG) now replay the
FlareSolverr-harvested User-Agent** (`current_ua` → `get_with_ua`) so the
`cf_clearance` cookie — bound to (IP, UA) — isn't rejected by Cloudflare on a
UA mismatch. If jmuse starts returning the "Just a moment" HTML on the *direct*
call (JSON parse error) even though the score page loads, suspect this UA
alignment first.

## The hardcoded fallback salt

`FALLBACK_SALT` (currently `9654,4e`) mirrors the value committed in
LibreScore/dl-librescore's `src/file.ts`
(`md5(`${id}${type}${index}9654,4e`).slice(0,4)`). It's tried after the
bundle-extracted salt, so a broken/renamed bundle chunk no longer kills
downloads. **When downloads break with `jmuse error` on every score and the
bundle salt looks wrong, first bump `FALLBACK_SALT` to dl-librescore's current
value** (grep their `file.ts` for `.slice(0, 4)`) — that's the fastest recovery
and needs no bundle-format reverse-engineering. Keep extracting the live salt
too; the constant is only the safety net.

## The token algorithm — the thing that matters

The `/api/jmuse` `Authorization` token is:

```
md5(score_id + media_type + index + salt)  -> hex, first 4 chars (lowercase)
```

- `media_type` is `"img"` for page PNGs.
- `index` is the 0-based page number, rendered as a decimal string.
- `salt` is a short string embedded in the JS bundle. It changes on every
  MuseScore deploy, which is why we extract it at runtime (`find_random_token`)
  rather than hard-coding it.

This is **plain MD5** — confirmed against multiple independent implementations:
- yt-dlp `musescore.py`: `md5((video_id + 'mp30gs')).hexdigest()[:4]`
- `amuse.py`: `md5(str(id) + format + str(section) + seed).hexdigest()[0:4]`
- `SCASO/scaso.py`: `md5(f"{score_id}{asset_type}{index}{seed}").hexdigest()[:4]`

⚠️ **Do NOT reintroduce a JS engine (Boa/QuickJS) to run MuseScore's bundle.**
That was the old approach and it broke on every MuseScore JS deploy (the
0.5 MB minified bundle would use syntax the engine couldn't parse). Native MD5
is immune to JS churn. Keep `mint_tokens` as native `md-5`.

## Diagnosis: start from the logs

Ask the user for the warning line (now logs the full error chain via `{:#}`):

```
fetch_pdf_bytes failed; falling back to external_url ... error=<CHAIN>
```

Map the `error=` chain to the failing step above. If you only see the
outermost context, confirm `pdf_handler` is logging `format!("{:#}", e)`.

If you can't get logs, the most likely culprits in order are:
`mint_tokens`/salt, then `extract_bundle_url`, then FlareSolverr.

## How to fix each break

- **Salt no longer found** (`find_random_token`): MuseScore changed the call
  site shape. Current logic finds the string literal immediately before
  `).substr(0, 4)`. If they renamed `substr` to `substring`/`slice` or changed
  the truncation length, update `substr_zero_four_follows` / `find_random_token`
  accordingly. Verify the extracted salt against a fresh bundle.
- **Token rejected** (`jmuse error`) but salt looks right: the input ordering
  or hash may have changed. Re-derive the formula from a current reference impl
  (yt-dlp is the most reliably maintained) before editing `mint_tokens`.
- **Bundle URL not found**: update `matches_bundle_pattern` to the new path shape.
- **Search returns nothing** (`neither JSON nor DOM extraction matched`):
  that's the search scraper (`extract_search_scores` / `_from_dom`), a separate
  concern from downloads. A "No results for 'X'" og:title in the snippet means
  the query legitimately had no hits — not a bug.

## Verify

- `cargo test mint_tokens` — pins the MD5 algorithm against known vectors
  (`md5("0")` → `cfcd`, `md5("1")` → `c4ca`). Must pass after any token change.
- `cargo build` — must be clean.
- Live smoke (needs FlareSolverr reachable, so run on the deployment / NAS,
  not a sandboxed CI box):
  `cargo test musescore_smoke -- --ignored --nocapture`
- End-to-end: redeploy, then `GET /pdf/musescore/<id>` should return a PDF
  (`%PDF-1.` magic bytes), not a 3xx redirect to musescore.com.

## Constraints

- Keep the build toolchain-light (no C compiler requirement). `md-5` is pure
  Rust — fine. Don't add a JS engine or native crypto that needs gcc.
- Don't hard-code the salt; always extract it from the live bundle.
