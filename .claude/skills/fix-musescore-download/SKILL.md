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

1. **`fetch_score_page`** → `fetch_html_challenged` (FlareSolverr / `cf_clearance`).
   Breaks if Cloudflare/FlareSolverr is down. Logs: `flaresolverr …`, `Just a moment`.
2. **`extract_bundle_url`** — find the `…/static/public/build/…/N.<hash>.js` URL
   in the page HTML. Breaks if MuseScore changes the bundle path shape
   (`matches_bundle_pattern`). Error: `could not find musescore bundle URL`.
3. **`extract_score_meta`** → `find_pages_count` — page count from the hydration JSON.
4. **`prepare_algorithm`** → `find_random_token` — extract the **salt** from the
   bundle. Error: `randomToken salt not found in musescore bundle`.
5. **`mint_tokens`** — compute the per-page token **natively** (MD5). See below.
6. **`jmuse_url`** per page → CDN PNG fetch → **`assemble_pdf`**.
   `musescore jmuse error: …` means the token/salt is wrong, OR the score is
   MuseScore-Pro-only content (expected for some scores — try another).

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
