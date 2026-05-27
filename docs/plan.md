# Sheet-music-browsarr — Sheet Music Manager + Public Search/Email Site

## Context

You want a Sonarr/Radarr equivalent for sheet music, hosted on your Cloudflare domain, that doubles as a **public-facing sheet music search & delivery service**. Capabilities:

1. **Public surface** at `music.<your-domain>`:
   - Search sheet music in English, Simplified Chinese, Traditional Chinese, and Hanyu Pinyin
   - Stream / browser-download PDFs from public sources (IMSLP) and from items already in your library
   - "Email me this PDF" — any recipient address, protected by Cloudflare Turnstile + rate limits
   - Trigger torrent downloads (Whatbox qBit → rsync to NAS), also Turnstile-gated and quota-limited
2. **Admin surface** (auth-gated): tracker config, qBit creds, rsync target, SMTP, abuse-control thresholds, queue + library moderation, audit log
3. One combined Rust binary (Sonarr-style single app), shipped as a multi-arch Docker image on `ghcr.io`, run on the NAS via Dockge/Portainer

Because the public surface now triggers real backend actions (sending email, consuming Whatbox quota, writing to your NAS), **abuse mitigation is core to the design, not an afterthought**. Cloudflare Turnstile + per-IP/per-recipient rate limits + per-action quotas + an optional admin-moderation queue are all in scope from Phase 1.

### Note on Rust as a speed argument

You said "maybe we can write it in rust so its faster". This workload is almost entirely I/O — language speed is essentially irrelevant. The real reasons Rust fits here: single static binary, ~30MB Docker image, no runtime install, low idle memory (~10MB vs 80MB+ Node/Python equivalents), strong typing prevents whole classes of bugs in the qBit/rsync/SMTP/multilingual plumbing.

---

## Architecture

```
                  ┌──────────────────────────────────────────────┐
   Browser ─────► │  Cloudflare edge                             │
                  │   • music.<your-domain>                      │
                  │   • Turnstile widget on download/email forms │
                  │   • Edge WAF, basic bot rules                │
                  └──────────────────────┬───────────────────────┘
                                         │ cloudflared tunnel
                                         ▼
   ┌──────────────────────────────────────────────────────────────────┐
   │  TerraMaster NAS (192.168.0.132)                                 │
   │  ┌────────────────────────────────────────────────────────────┐  │
   │  │ sheet-music-browsarr (Docker, port 8686)                               │  │
   │  │   Inbound HTTP                                             │  │
   │  │     ├─ tower middleware: rate limit (per-IP) + Turnstile   │  │
   │  │     │  verification on POST /download, /email              │  │
   │  │     ├─ axum router                                         │  │
   │  │     │   ├─ public:  /, /search, /pdf/:id, /download, /email│  │
   │  │     │   └─ admin:   /admin/* (session auth)                │  │
   │  │     └─ workers (tokio tasks)                               │  │
   │  │         ├─ queue state machine                             │  │
   │  │         ├─ qbit poller                                     │  │
   │  │         ├─ rsync runner                                    │  │
   │  │         ├─ email sender (lettre)                           │  │
   │  │         └─ quota enforcer (storage cap, Whatbox concurrent)│  │
   │  │   SQLite at /config/sheet-music-browsarr.db                            │  │
   │  └────────┬──────────────────────┬───────────────────────────┘   │
   │           │ HTTPS                │ exec rsync over SSH            │
   │           ▼                      │                                │
   │   IMSLP / Torznab indexers       │                                │
   │   (outbound only)                ▼                                │
   │   Library: /Volume1/media/sheet-music/                            │
   └───────────────────────────────────────────────────────────────────┘
                                         ▲
                                         │  rsync pull over SSH
                                         │
   ┌─────────────────────────────────────┴─────────────────────────┐
   │  Whatbox (feifei.box.ca)                                      │
   │   • qBittorrent Web API                                       │
   │   • SSH/rsync access                                          │
   │   • Completed downloads → ~/files/sheet-music/                │
   └───────────────────────────────────────────────────────────────┘
```

### Component summary

| Component                 | Where                                   | Purpose                                                                    |
| ------------------------- | --------------------------------------- | -------------------------------------------------------------------------- |
| sheet-music-browsarr binary           | NAS, Docker, port 8686                  | Web UI + API + workers + abuse controls, all in one Rust binary            |
| Cloudflared tunnel route  | Existing cloudflared container          | Maps `music.<your-domain>` to `terramaster:8686`                           |
| Cloudflare Turnstile      | Cloudflare dashboard                    | Bot/abuse protection on POST endpoints; verified server-side               |
| SQLite DB                 | `/Volume1/docker/sheet-music-browsarr/config/`      | Settings, indexers, queue, library, audit log, rate-limit buckets          |
| Library                   | `/Volume1/media/sheet-music/`           | Final PDFs land here                                                       |
| SSH key for Whatbox       | `/Volume1/docker/sheet-music-browsarr/config/ssh/`  | Mounted into the container for rsync                                       |
| GitHub repo + Actions     | `github.com/<you>/sheet-music-browsarr`             | Builds `ghcr.io/<you>/sheet-music-browsarr:<tag>` multi-arch (linux/amd64 first)       |

---

## Tech stack

| Concern                  | Choice                       | Why                                                                                    |
| ------------------------ | ---------------------------- | -------------------------------------------------------------------------------------- |
| Web framework            | `axum` + `tower`             | De facto Rust standard, async, ecosystem support                                       |
| Async runtime            | `tokio`                      | Required by axum                                                                       |
| Database                 | SQLite via `sqlx`            | Single-file, perfect for personal-scale; compile-time checked queries                  |
| Migrations               | `sqlx::migrate!`             | Migrations in `migrations/` checked at startup                                         |
| HTTP client              | `reqwest`                    | For IMSLP, qBit API, Torznab feeds, Turnstile verification                             |
| Templating               | `askama`                     | Typed templates compiled into the binary                                               |
| Frontend                 | HTMX + Tailwind (built once) | No JS toolchain at runtime; server-rendered partials with `hx-get` / `hx-post`         |
| Auth (admin)             | `tower-sessions` + Argon2    | Single admin user, signed session cookie. Initial password via env var                 |
| Email                    | `lettre`                     | SMTP, STARTTLS, attachments                                                            |
| Bot protection           | Cloudflare Turnstile + `reqwest` verifier | Edge widget, server-verified token; no external SaaS to add                 |
| Rate limiting            | `tower-governor` or custom DB-backed counters | Per-IP and per-action limits. DB-backed lets limits survive restarts  |
| Chinese tokenization     | `jieba-rs`                   | Segments Chinese text for query matching                                               |
| Pinyin conversion        | `pinyin` crate               | Hanzi → Pinyin for indexing/matching                                                   |
| Simplified ↔ Traditional | `opencc-rust` (bundled OpenCC dicts) | Bidirectional conversion so a query in either script hits both           |
| Static embedding         | `rust-embed`                 | HTMX/CSS bundled into the binary                                                       |
| Logging                  | `tracing` + `tracing-subscriber` | Structured JSON logs to stdout                                                     |
| Secrets at rest          | `aes-gcm` with key from env  | qBit/SMTP passwords encrypted in SQLite                                                |
| SSH/rsync                | Shell out to `rsync` + `ssh` binaries in the container | OpenSSH client is more reliable than Rust SSH crates           |

**Frontend rationale**: HTMX + Tailwind (built once, output CSS embedded) gives a real interactive UI without a JS runtime/bundler. Build-time Tailwind is fine because the only one running it is the GitHub Actions pipeline.

---

## Routes & auth model

| Path                            | Method   | Auth      | Turnstile? | Rate-limited                  | Purpose                                            |
| ------------------------------- | -------- | --------- | ---------- | ----------------------------- | -------------------------------------------------- |
| `/`                             | GET      | public    | no         | per-IP soft                   | Search page                                        |
| `/search?q=...`                 | GET      | public    | no         | per-IP soft                   | HTMX partial: result cards                         |
| `/pdf/:library_id`              | GET      | public    | no         | per-IP soft                   | Stream a library PDF to the browser                |
| `/pdf/imslp/:imslp_id`          | GET      | public    | no         | per-IP soft                   | Proxy-stream an IMSLP PDF (sheet-music-browsarr fetches)       |
| `/email`                        | POST     | public    | **yes**    | per-IP + per-recipient daily  | Send PDF to user-supplied email address            |
| `/download`                     | POST     | public    | **yes**    | per-IP daily + global concurrency | Add a torrent to Whatbox queue                 |
| `/admin`                        | GET      | **auth**  | no         | —                             | Admin dashboard                                    |
| `/admin/queue`                  | GET      | **auth**  | no         | —                             | Current queue + history + moderation              |
| `/admin/library`                | GET      | **auth**  | no         | —                             | Completed downloads                                |
| `/admin/settings`               | GET/POST | **auth**  | no         | —                             | qBit, rsync, SMTP, abuse thresholds, Turnstile keys|
| `/admin/indexers`               | GET/POST | **auth**  | no         | —                             | CRUD for Torznab indexers                          |
| `/admin/audit`                  | GET      | **auth**  | no         | —                             | Audit log: public-triggered actions w/ IP + UA     |
| `/login` / `/logout`            | —        | —         | no         | —                             | Admin auth                                         |
| `/healthz`                      | GET      | public    | no         | —                             | Liveness                                           |

**"Soft" rate limit** = generous (e.g., 60/min) to deter scraping but not get in the way. **Strict limits** on POSTs: per-IP daily, per-recipient daily for email, global concurrent for downloads.

---

## Anti-abuse model (Phase 1+ — not deferrable)

Layered defenses on the three public POST surfaces (`/email`, `/download`):

### Layer 1 — Cloudflare edge
- Turnstile widget rendered in the form; server-verified token on every POST
- Standard Cloudflare bot fight mode + a basic WAF rule blocking obvious scraper UAs
- Country-block list optional (admin setting)

### Layer 2 — sheet-music-browsarr middleware
- `tower-governor` per-IP rate limit (e.g., 60 req/min global, 10 search/min)
- Turnstile token verification middleware on `/email` and `/download` (reject if missing/invalid/replayed)

### Layer 3 — application-level quotas (configurable in admin settings)
- **Email per IP**: 10/day default
- **Email per recipient address**: 5/day default (prevents bombing a single inbox)
- **Email global**: 200/day default (SMTP cap headroom)
- **Download per IP**: 5/day default
- **Concurrent torrent downloads**: 5 default (don't saturate Whatbox)
- **Max single torrent size**: 200MB default (sheet music PDFs are tiny; large = probably misuse)
- **Library storage cap**: 50GB default (hard stop on new torrents when approaching)

### Layer 4 — moderation queue (optional, admin toggle)
- `settings.public_downloads_require_approval = true|false`
- When `true`: public-triggered torrents enter `pending_approval` state. Admin sees them in `/admin/queue` and clicks Approve before they hit Whatbox. Great safety net for the first few weeks.
- IMSLP / library-PDF downloads bypass this (no resource consumption — IMSLP files are public, library PDFs already exist).

### Layer 5 — post-completion content validation
- Completed file must be one of: `.pdf`, `.zip`/`.rar` containing only `.pdf`s, `.mscz` (MuseScore)
- Anything else → quarantine, don't move to library, log
- Prevents the obvious "download something stupid to your NAS" attack

### Layer 6 — audit log
- Every public POST recorded with: IP, user-agent, Turnstile challenge metadata, action, target item, result
- Surfaced at `/admin/audit` with filters
- IP-ban CTA on each row (writes to a small `banned_ips` table, middleware refuses those)

---

## Multilingual search (English / 简体 / 繁體 / Pinyin)

Goal: a user typing `xiaobang` or `肖邦` or `蕭邦` or `Chopin` should find the same composer.

### Pipeline

1. **Detect input scripts** in the query string:
   - Han characters → `simplified` and/or `traditional` (use OpenCC to convert each Han token to the other variant, so we have both)
   - ASCII letters → check against pinyin syllable patterns + alias dictionary
   - Mixed inputs are normal (e.g., "Chopin 夜曲") — handle each token independently

2. **Expand via alias dictionary** — a curated CSV bundled into the binary:
   ```
   en,simplified,traditional,pinyin
   Chopin,肖邦,蕭邦,xiaobang
   Bach,巴赫,巴赫,bahe
   Beethoven,贝多芬,貝多芬,beiduofen
   Mozart,莫扎特,莫扎特,mozhate
   piano,钢琴,鋼琴,gangqin
   violin,小提琴,小提琴,xiaotiqin
   cello,大提琴,大提琴,datiqin
   nocturne,夜曲,夜曲,yequ
   sonata,奏鸣曲,奏鳴曲,zoumingqu
   ...
   ```
   - Initial seed: ~200 common composers + ~50 instruments + ~50 form names. Curated by hand, lives in `assets/zh_aliases.csv`, easy to extend.
   - At search time, every token is looked up; matches expand to all known variants.

3. **Query upstream sources with variants in parallel**:
   - IMSLP gets queried with each variant separately, results merged + deduped on (composer, title)
   - Torznab indexers same approach (most trackers index English titles + occasionally CJK; querying both maximizes hits)

4. **Display normalization**: render the canonical English name in the UI title, with the original-script form shown as a secondary line if it differs from query.

### Crate dependencies
- `pinyin = "0.10"` — Hanzi → Pinyin
- `jieba-rs = "0.7"` — Chinese segmentation (for splitting "肖邦夜曲" into ["肖邦", "夜曲"])
- `opencc-rust = "1.1"` — Traditional ↔ Simplified

### Module layout
```
src/i18n/
├── mod.rs           # query expansion entry point
├── detect.rs        # script detection
├── alias.rs         # CSV loader + lookup
└── opencc.rs        # thin wrapper around opencc-rust
```

### Limitations to acknowledge
- Free-form pinyin → Hanzi for arbitrary words is genuinely hard (segmentation ambiguity). The alias dictionary covers known names — this is the realistic scope.
- IMSLP's coverage of CJK metadata is uneven. Some pieces are only indexed in English. The merge step handles this gracefully (results from any variant get returned).

---

## Data model (SQLite)

```sql
CREATE TABLE settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL   -- JSON-encoded; secrets AES-GCM-encrypted before storing
);
-- Includes: qbit_url, qbit_user, qbit_pass, rsync_host, rsync_path,
--   rsync_dest_path, smtp_host, smtp_port, smtp_user, smtp_pass,
--   library_path, turnstile_site_key, turnstile_secret_key,
--   public_downloads_require_approval (bool), abuse quotas (json blob)

CREATE TABLE indexers (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('torznab', 'imslp')),  -- imslp is a built-in singleton row
  url TEXT, api_key TEXT,
  categories TEXT,         -- JSON array
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL
);

CREATE TABLE queue_items (
  id INTEGER PRIMARY KEY,
  title TEXT NOT NULL,
  source TEXT NOT NULL,           -- 'imslp' | 'torrent'
  source_url TEXT NOT NULL,
  state TEXT NOT NULL,            -- pending_approval|pending|fetching|downloading|completed|rsyncing|quarantined|done|failed
  qbit_hash TEXT,
  local_path TEXT,
  size_bytes INTEGER,
  progress REAL,
  error TEXT,
  triggered_by_ip TEXT,           -- null if admin
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE library_items (
  id INTEGER PRIMARY KEY,
  queue_item_id INTEGER REFERENCES queue_items(id),
  title TEXT NOT NULL,
  path TEXT NOT NULL,
  composer TEXT, instrument TEXT,
  size_bytes INTEGER NOT NULL,
  public_visible INTEGER NOT NULL DEFAULT 1,  -- admin can hide
  added_at TEXT NOT NULL
);

CREATE TABLE audit_log (
  id INTEGER PRIMARY KEY,
  ts TEXT NOT NULL,
  ip TEXT NOT NULL,
  user_agent TEXT,
  action TEXT NOT NULL,           -- search|download|email|admin_login|...
  target TEXT,                    -- item id or recipient
  result TEXT NOT NULL,           -- ok|rate_limited|turnstile_failed|quarantined|...
  meta TEXT                       -- JSON freeform
);

CREATE TABLE rate_buckets (
  bucket_key TEXT PRIMARY KEY,    -- e.g. "ip:1.2.3.4:email" or "recipient:foo@bar.com:email"
  window_start TEXT NOT NULL,
  count INTEGER NOT NULL
);

CREATE TABLE banned_ips (
  ip TEXT PRIMARY KEY,
  reason TEXT,
  banned_at TEXT NOT NULL
);

CREATE TABLE admin_user (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  password_hash TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
```

---

## Source integrations

### IMSLP

1. **Search**: query `https://imslp.org/api.php?action=opensearch&search=...&format=json` for autocomplete results, then `action=parse` to enrich. Fall back to scraping the search page with `scraper` crate when API is insufficient.
2. **PDF fetch**: results link to Petrucci library URLs like `https://imslp.org/wiki/Special:ImagefromIndex/<id>/...`. GET → either stream to the user (public download) or write to `/library` (admin add-to-library).

### MuseScore (added during build)

MuseScore.com is a community-uploaded sheet-music site. Free uploads have no server-side PDF (`is_pdf == 0`); we have to reconstruct the PDF ourselves from per-page PNG renderings the site serves via a token-gated API. The technique is a server-side port of the `musescore-downloader` browser extension — fragile by design but the only path that returns inline PDFs without paying for MuseScore Pro.

#### Search → click flow

User types a query → `/search` fan-out hits `Musescore::search` with each i18n variant → MuseScore page returns HTML with embedded hydration JSON → we parse the `scores` array (id, title, composer_name, thumbnail_url, href) → results merged with IMSLP + Mutopia, deduped by `(source, id)`, rendered as cards.

User clicks "Open PDF" on a MuseScore card → browser GETs `/pdf/musescore/{score_id}` → `pdf_handler` (`src/routes/public.rs`) dispatches to `Musescore::fetch_pdf_bytes(score_id, MAX_PDF_BYTES)` → on success: inline PDF with `Content-Type: application/pdf` + `Content-Disposition: inline; filename="{id}.pdf"`. On any failure: silent 302 to `https://musescore.com/score/{id}` (Phase G replaces this with an inline failure page that lets the user choose).

#### `fetch_pdf_bytes` pipeline (5 stages)

1. **Score page fetch** — GET `https://musescore.com/score/{id}`. Extract:
   - **Bundle URL** via `extract_bundle_url`. Looks for `<link rel="preload" as="script" href="…/static/public/build/…/<digits>.<hash>.js">`. Helper `matches_bundle_pattern` filters out `ms.<hash>.js` and `vendor.<hash>.js`.
   - **Score meta** (`pages_count`, etc.) via hydration JSON.

2. **Bundle prepare** — `prepare_algorithm(bundle_url)`. Cached by `bundle_url` in a `Mutex<Option<CachedAlgorithm>>` — bundle URLs embed a content hash, so a single cache slot is enough and invalidates automatically on MuseScore deploys. Fetches ~0.5 MB of minified JS and rewrites it via three textual substitutions:
   - `find_random_token` — regex over the bundle finding the literal `"<salt>".substr(0, 4)` to extract the per-deploy `randomToken` salt.
   - `find_md5_module_id` — scans for `_digestsize` / `_blocksize` (pycryptodome-style literals) then walks backwards to a webpack module header `, <id>: function(` or `, <id>: (`. The id locates the MD5 module among hundreds in the bundle.
   - Three rewrites — `replace_webpack_header`, `replace_closing_paren`, `replace_exports_with_window` — turn the MD5 module's `module.exports` into a globally-callable `window.generateToken`.

3. **Per-page token mint + jmuse URL resolve** (serial). For each `index in 0..pages_count`:
   - `mint_token`: `tokio::task::spawn_blocking` constructs a `boa_engine::Context`, evals `var window = {};` then the rewritten bundle, calls `window.generateToken(score_id + "img" + index + random_token)`, takes `.substring(0, 4)` for the 4-char Authorization token.
   - `jmuse_url`: GET `/api/jmuse?id={score_id}&index={index}&type=img` with `Authorization: {token}` and `Referer: {external_url}`. Response is JSON `{result: "success", info: {url: <cdn_url>}}`. The CDN URL points at a per-page PNG.
   - Comment at the loop notes: "musescore's per-IP rate limit on /api/jmuse is hair-trigger; serial keeps the failure modes predictable" — parallelizing here is left for later (a `buffer_unordered(4)` would give a ~4× speedup but only worth attempting after the pipeline is reliable).

4. **PNG fetch** (serial). Each CDN URL is fetched through `fetch_bytes(url, per_page_budget)` with a streaming size cap. Aggregate cap: `running` bytes must not exceed `max_bytes` across all pages.

5. **PDF assembly** — `tokio::task::spawn_blocking(|| assemble_pdf(&pngs))`. For each PNG: `printpdf::RawImage::decode_from_bytes`, derive page size from pixel dimensions at 96 DPI, create a `PdfPage` containing an `Op::UseXobject` for the image. Save with `PdfSaveOptions::default()`. Final size-cap check then `Ok(Vec<u8>)`.

#### Callers of `fetch_pdf_bytes`

Same method is invoked from three places with different size caps — any cache or fallback change must be coherent across all three:

- **`/pdf/musescore/{id}`** — public, `MAX_PDF_BYTES = 10 MB` (`src/routes/public.rs`). Inline PDF served to the browser; 302 fallback on error.
- **`/email`** — public, same 10 MB cap. PDF attached to the outbound SMTP message; Turnstile + rate-limit gated.
- **`/admin/library/add`** — admin-only, `ADMIN_MAX_PDF_BYTES = 25 MB` (`src/routes/admin.rs`). Result written to `/library/{title}.pdf` + DB rows in `queue_items` + `library_items`.

#### Failure modes by stage

The status-code surfacing fix (commit `f343096`) makes every failure carry the actual HTTP code + a 200-char body snippet. Maps to the reliability roadmap (next section):

| Stage | Symptom in logs | Likely cause | Roadmap phase |
|---|---|---|---|
| Search | `musescore search HTTP 403: <Cloudflare HTML>` | Cloudflare bot-block | **B** (browser headers) → **C** (FlareSolverr) |
| Search | `musescore search HTTP 429` | Rate limit (4× i18n fan-out amplifies) | Bump moka TTL for MuseScore |
| Search | `musescore search hydration not found` | MuseScore changed HTML layout | Update `find_hydration_json` |
| Score page | `could not find musescore bundle URL on …` | Preload tag moved | Update `matches_bundle_pattern` |
| Bundle prepare | `randomToken salt not found in bundle` | `substr(0, 4)` site restructured | **E** (regex update) |
| Bundle prepare | `MD5 module not found in bundle` | `_digestsize` literal gone or webpack header changed | **E** — fallback: drop boa, use Rust `md-5` crate directly with the extracted salt |
| Token mint | `window.generateToken missing` | Rewrite didn't produce expected global | **E** |
| jmuse | `musescore jmuse error: …` | Token rejected by server | Wrong formula (`id+type+index+salt`) or Pro-only content |
| PNG fetch | non-2xx | CDN gated; same Cloudflare path as search | **B/C** |
| PDF assembly | `bytes don't start with %PDF-1.` | printpdf re-encode regression | **E** (verify passthrough) |
| Whole pipeline | every "Open PDF" silently 302s | No successful click yet — pipeline could be entirely broken | Phase D smoke test surfaces which stage |

#### Caches involved

- `cache.rs` moka **search** cache — keyed by `(source_id, query_variant)`, 60 s TTL, 1000-entry LRU. Hits count as "ok" for `SourceHealth` (recent fresh response is good liveness signal).
- `Musescore.cached: Mutex<Option<CachedAlgorithm>>` — one slot for the prepared (rewritten) bundle keyed by `bundle_url`. Reused for every page of every subsequent score until MuseScore deploys a new bundle.
- `cache.rs` moka **`ThumbnailCache`** — 24 h TTL, 5000-entry cap. Used by IMSLP's lazy `/thumbnail/{source}/{id}` route; MuseScore's `thumbnail_url` is already in the search hydration so no lazy lookup needed.
- **Planned (Phase F)** — on-disk PDF cache at `/cache/musescore/{cache_version}/{id}-{bundle_hash_prefix}.pdf`. Bundle-hash-keyed so MuseScore deploys auto-invalidate. `cache_version` bumped when the rewriter changes. Atomic `.tmp` + rename. Separate `/cache` volume so admins can `rm -rf` without touching `/config`.

#### Source health surfacing (Phase G.0 — shipped)

`src/sources/health.rs`. In-memory `Arc<HashMap<&'static str, RwLock<SourceHealth>>>` map built once in `main.rs`. `record_ok` / `record_err` called inside the search fan-out and inside `pdf_handler` after `fetch_pdf_bytes`. `is_degraded()` returns true when `consecutive_fails >= 4` (one full i18n fan-out worth of failures). `/admin/sources` (`templates/admin/sources.html`) renders the live snapshot plus an audit-log-backed table of per-source `pdf` and `library_add` outcomes — durable history complementing the volatile in-memory map.

### Torznab indexers (native)

Each indexer = `(name, url, api_key, categories)`. Search worker:
1. `GET {url}?t=search&apikey={key}&q={query}&cat={cats}` per enabled indexer (in parallel via `futures::join_all`)
2. Parse Torznab XML with `quick-xml`
3. Merge into unified `SearchResult` with `source: "torrent"`, attach `download_url` (torrent file or magnet)

Trackers don't usually have a "sheet music" category; you'll filter via query keywords plus indexer-specific category IDs you configure.

### qBittorrent on Whatbox

1. POST `/api/v2/auth/login` with creds (also handles HTTP basic auth layer at Whatbox edge)
2. POST `/api/v2/torrents/add` with the `.torrent` content or magnet, `savepath = /home/<user>/files/sheet-music/`, set a known category for filtering
3. Poll `/api/v2/torrents/info?hashes={hash}` every 30s until done
4. Queue worker transitions to `rsyncing`

### Rsync pull from Whatbox → NAS

```
rsync -avz --remove-source-files \
  -e "ssh -i /config/ssh/id_ed25519 -o StrictHostKeyChecking=accept-new" \
  <user>@feifei.box.ca:/home/<user>/files/sheet-music/<file> \
  /library/
```

`--remove-source-files` keeps Whatbox tidy. If qBit complains about missing files while still seeding, drop that flag and add a periodic cleanup worker that removes only items past their seed time.

### Email (public + admin)

`/email` POST flow:
1. Verify Turnstile token (server-side, with site key + secret from settings)
2. Check rate buckets: `ip:<ip>:email` and `recipient:<addr>:email`; reject with 429 + audit-log row if exceeded
3. Resolve PDF: must be in `library_items` (public-allowed) or IMSLP direct fetch
4. Build email via `lettre`, attach PDF (max 10MB attachment cap), send via SMTP
5. Audit-log the action with result

Optional admin setting: `email_on_complete` — when a torrent the admin queued finishes, email it to the admin address automatically.

---

## Deployment

### Repo layout

```
sheet-music-browsarr/
├── Cargo.toml
├── build.rs                       # builds Tailwind output CSS at build-time
├── src/
│   ├── main.rs
│   ├── config.rs                  # env parsing, secret-at-rest helpers
│   ├── db.rs
│   ├── auth/
│   ├── routes/
│   │   ├── public.rs              # /, /search, /pdf, /email, /download
│   │   └── admin.rs               # /admin/*
│   ├── middleware/
│   │   ├── rate_limit.rs
│   │   ├── turnstile.rs
│   │   └── audit.rs
│   ├── sources/
│   │   ├── imslp.rs
│   │   └── torznab.rs
│   ├── workers/
│   │   ├── queue.rs               # state machine driver
│   │   ├── qbit.rs
│   │   ├── rsync.rs
│   │   ├── email.rs
│   │   └── quota.rs
│   ├── i18n/
│   │   ├── mod.rs
│   │   ├── detect.rs
│   │   ├── alias.rs
│   │   └── opencc.rs
│   └── models/
├── migrations/
├── templates/
│   ├── base.html
│   ├── search.html
│   ├── result_card.html           # HTMX partial
│   ├── email_form.html
│   └── admin/
├── static/
│   └── htmx.min.js
├── assets/
│   ├── zh_aliases.csv
│   └── tailwind.css               # source; compiled at build time
├── Dockerfile
└── .github/workflows/release.yml
```

### Dockerfile (sketch)

Three-stage build:
1. **css-builder** — `node:slim`, runs Tailwind once over `templates/**` + `assets/tailwind.css` → `dist/styles.css`
2. **rust-builder** — `rust:1-bookworm`, copies in `dist/styles.css`, `cargo build --release`
3. **runtime** — `debian:bookworm-slim`, installs `rsync`, `openssh-client`, `ca-certificates`, copies binary, `EXPOSE 8686`

Final image: ~35MB. Stays small thanks to `rust-embed` + the bundled binary.

### GitHub Actions

On push to `main` and tags:
1. `docker/setup-buildx-action`
2. `docker/login-action` against `ghcr.io` (using `GITHUB_TOKEN`)
3. Build + push `ghcr.io/<you>/sheet-music-browsarr:latest` and `:sha-<short>` (linux/amd64 first; arm64 later via QEMU)
4. On version tags: also `:v<x.y.z>`

### Dockge stack on NAS

`/Volume1/docker/sheet-music-browsarr/docker-compose.yml`:
```yaml
services:
  sheet-music-browsarr:
    image: ghcr.io/<you>/sheet-music-browsarr:latest
    container_name: sheet-music-browsarr
    restart: unless-stopped
    ports:
      - "8686:8686"
    environment:
      - TZ=America/Los_Angeles
      - PUID=1000
      - PGID=1000
      - BROWSARR_DB_PATH=/config/sheet-music-browsarr.db
      - BROWSARR_LIBRARY_PATH=/library
      - BROWSARR_SECRET_KEY=<long-random-string>
      - BROWSARR_ADMIN_PASSWORD=<set-on-first-run-only>
      - BROWSARR_SSH_KEY_PATH=/config/ssh/id_ed25519
    volumes:
      - /Volume1/docker/sheet-music-browsarr/config:/config
      - /Volume1/media/sheet-music:/library
```

### Cloudflared route

Add to the existing cloudflared config:
```yaml
- hostname: music.<your-domain>
  service: http://terramaster:8686
```
Then `cloudflared tunnel route dns <tunnel-id> music.<your-domain>`. No Cloudflare Access policy (auth + Turnstile live in sheet-music-browsarr).

---

## Phased build plan

Each phase is independently shippable.

### Status (as of 2026-05-27)

**Shipped to `main` and deployed:** Phases 0, 1, 2, 3, plus a chunk of work that wasn't in the original phasing (the multi-source refactor, Mutopia, MuseScore, server cache, typeahead UI patterns, fuzzy alias matching — all described under "Shipped beyond the original plan" below).

**Deferred / potential extensions:** Phases 4, 5, 6, 7, 8. The catalog gap they would fill (method books, fake books, modern transcriptions beyond MuseScore's user uploads, choral/orchestral parts not in IMSLP) is real but narrow for the typical "I want Chopin's Nocturne" use case. Decision to defer reached after evaluating cost (~2 weekends + permanent operational tax of stateful workers + Whatbox/qBit/rsync flakiness) vs. benefit (catalog extension for a niche of repertoire that MuseScore already partially covers). Revisit when concrete missing-catalog queries pile up in the audit log.

### Phase 0 — Skeleton (target: 1 evening)
- `cargo new sheet-music-browsarr`, wire `axum + sqlx + askama + tower-sessions`
- Empty migrations, healthz, admin login
- Dockerfile builds; GH Actions publishes to ghcr.io
- Dockge stack deployed; reachable at `terramaster:8686`
- Cloudflared route added; reachable at `music.<your-domain>`

**Verify**: log into `/login`, land on `/admin`.

### Phase 1 — IMSLP search (public) + admin download (target: 1 weekend)
- `sources/imslp.rs` returns search results
- `/search` (public) with HTMX form; `/pdf/imslp/:id` (public) proxies and streams the PDF to the browser
- `/admin` admin form: paste an IMSLP result, it lands in the library
- `audit_log` populated for every public hit
- Per-IP soft rate limit on `/search` (no Turnstile yet — those endpoints don't trigger side effects)

**Verify**: search "Chopin Nocturne" on the public site, click a result, PDF streams in the browser; admin clicks "save to library", PDF lands in `/Volume1/media/sheet-music/`.

### Phase 2 — Public email + Turnstile + rate limits (target: 1 weekend)
- Settings page for SMTP creds + Turnstile site/secret keys + quota knobs
- `/email` POST: Turnstile verify → rate-limit checks → fetch PDF → send via lettre → audit
- Email form on each search result card ("email this to me")
- Frontend renders Turnstile widget; CSP set up so it loads

**Verify**: submit your own email → arrives with PDF; hammer it from a script and watch quotas reject after the threshold; audit log shows the attempts.

### Phase 3 — Multilingual search (target: 1 weekend) — SHIPPED
- `src/i18n/` module + `assets/zh_aliases.csv` (~60 curated rows: composers, instruments, common form names with `en, simplified, traditional, pinyin` columns)
- Search pipeline tokenises the query, looks each token up in the alias index, emits up to 4 script variants, fans out (source × variant) in parallel, dedupes results by `(source, id)`
- Pure-Rust implementation — `OpenCC` (C library) and `jieba` / `pinyin` (heavier deps) deferred. Free-form CJK without whitespace and phonetic Pinyin-to-unknown-word conversion are known limitations
- Bonus: Damerau-Levenshtein fuzzy fallback on the alias lookup ("Chpin", "Bethovan" still match), inline tests for both exact and fuzzy paths

---

## Shipped beyond the original plan

These pieces weren't on the original phasing but landed during build-out. Listed here so the deferred-phases section below isn't read as "everything else not yet done".

- **Multi-source refactor.** `src/sources/mod.rs` defines an async `Source` trait (`id`, `display_name`, `external_url`, `search`, `fetch_pdf_bytes`); `AppState` holds `Vec<Arc<dyn Source>>`; the search and PDF routes generalise on a `source_id` path-param. New sources are drop-in additions.
- **Mutopia source.** Scrapes the classic CGI search HTML; base64-encodes the direct PDF URL as the public id so single-segment path params stay clean.
- **MuseScore source.** Server-side port of the `musescore-downloader` browser extension's technique; ~830 lines in `src/sources/musescore.rs`. Full pipeline reference + click flow in `## Source integrations` → `### MuseScore (added during build)` above. In-flight reliability work tracked in `## MuseScore reliability roadmap (Phases A→G)` below.
- **Server-side search cache** (`moka`). Per `(source_id, query_variant)` tuple, 60 s TTL, 1000-entry LRU cap. Cross-user repeat queries hit memory; protects rate-limited upstreams (especially IMSLP, which our Phase 3 fan-out amplifies 4×).
- **Typeahead UI patterns.** HTMX `hx-trigger="keyup changed delay:300ms"` for debounce, `hx-sync this:replace` for race-condition cancellation, `hx-indicator` for a fade-in "Searching…" affordance, 2-char minimum on the server, `Cache-Control: public, max-age=60` for browser caching. Patterns lifted from the Databricks FE-SYS typeahead playbook.
- **CI → Portainer redeploy webhook.** `gh secret PORTAINER_WEBHOOK_URL` + a final `curl -X POST` step in `release.yml`. Push to main → build → publish to `ghcr.io` → POST webhook → Portainer pulls and recreates the container. `pull_policy: always` in compose makes the redeploy actually fetch fresh.
- **IMSLP disclaimer handling.** Two-hop fetch: when the work-page scraper finds a `Special:ImagefromIndex/...` URL (disclaimer-gated), the source follows it once and scrapes the embedded CDN URL out of the disclaimer page itself. Pre-seeded `imslpdisclaim` accept cookie as a belt-and-suspenders. `Content-Type` check on the upstream response so HTML-disguised-as-PDF can't be served to the browser.

---

## MuseScore reliability roadmap (Phases A→G)

Distinct from the original Phase 0–8 phasing; this is the in-flight effort to make MuseScore actually deliver inline PDFs reliably. Lives in its own section because (a) it spans search reliability and PDF delivery as a single feature, (b) several phases are conditional on what diagnostics reveal, and (c) the original implementer never smoke-tested end-to-end. Detailed working plan: `C:\Users\georg\.claude\plans\curious-frolicking-wave.md`.

Phasing chosen to keep variables independent during diagnosis — don't change HTTP fingerprint before measuring what's broken, and don't blame the bundle rewriter until the fetch is unblocked.

### Phase A — Diagnose (no code)

1. **Audit-log triage** on the NAS:
   ```sql
   SELECT result, COUNT(*) FROM audit_log
   WHERE action='pdf' AND target LIKE 'musescore/%'
   GROUP BY result;
   ```
   All `fetch_failed_redirect` → PDF pipeline never worked in production (likely rewriter is stale). Any `ok` rows → pipeline did work; recent failures are search-side regression.
2. **Fresh log capture** — one search post-redeploy of the status-code-surfacing fix captures the actual HTTP status (`403` / `429` / `200`+selector-miss).
3. **Container-side curl** — `docker exec sheet-music-browsarr curl -v -H "User-Agent: …" 'https://musescore.com/sheetmusic?text=bach'`. If curl passes from the same IP and our reqwest doesn't, the gap is TLS/JA3 / HTTP/2 settings — FlareSolverr becomes the only path. Also check `Content-Encoding`: if `br`-only and we don't have the `brotli` reqwest feature, fetch returns garbage.

### Phase G.0 — Health scaffold (SHIPPED — commit `368e7ee`)

In-memory `SourceHealth` map (see Source integrations → "Source health surfacing") + `/admin/sources` page. Landed first as debugging infrastructure for the rest of the roadmap. No user-facing surface yet — that's Phase G.

### Phase D — Smoke test scaffold (SHIPPED locally — commit `a0b75fb`, awaits push)

`#[tokio::test] #[ignore]` at the bottom of `src/sources/musescore.rs` that exercises the whole pipeline against the live MuseScore site. CI runs it on every push (`musescore-smoke` job in `release.yml`) with `continue-on-error: true` so the inherently-flaky upstream never blocks deploy. Failure-mode mapping is documented inline so the panic message points at the right phase to fix. `MUSESCORE_SMOKE_QUERY` / `MUSESCORE_SMOKE_ID` env vars let us pin to a known-stable score.

Run manually:
```bash
cargo test --ignored musescore_smoke -- --nocapture
```
Linux only — the Windows dev host doesn't have gcc to link boa.

### Phase B — Browser-grade HTTP client + brotli

Apply realistic browser headers + cookie jar to the `Musescore` reqwest client. Mirror the IMSLP pattern at `src/sources/imslp.rs`.

- Add `"brotli"` to the reqwest features in `Cargo.toml` (MuseScore CDN often serves `br`-encoded responses; without the feature we get garbage).
- Realistic UA — current Chrome on desktop. (A Mozilla/5.0 Win64 UA shipped in an earlier commit; round out with `Accept`, `Accept-Language: en-US,en;q=0.9`, `Sec-Ch-Ua-*`, `Sec-Fetch-Site: none`, `Sec-Fetch-Mode: navigate`, `Sec-Fetch-User: ?1`, `Sec-Fetch-Dest: document`.)
- `Arc<Jar>` cookie store (no pre-seed — FlareSolverr in Phase C populates it).
- Same client for **every** musescore.com call (search, score page, bundle, jmuse, PNG fetch).

### Phase E — Bundle rewriter fixes + Boa context reuse

Two interleaved goals, both driven by Phase D output.

**(i) Rewriter fixes** — work backwards from whatever stage the smoke test fails at. Likely culprits: `find_random_token`, `find_md5_module_id`, the three replacements in `rewrite_bundle`. Fallback if MuseScore moved to WASM-MD5 or a different module loader: drop Boa entirely, extract `randomToken`, compute MD5 in Rust via the `md-5` crate.

**(ii) Boa context reuse** — change `mint_token` from one Boa `Context` per page to one per score. Currently a 50-page score = 50 × 500 KB string clones + 50 webpack-bundle re-evals; per-score reuse cuts that to 1 × 500 KB. Use `Arc<String>` for `prepared_js` to avoid even the per-score clone. The Plan agent flagged this as the only real perf concern in the pipeline; independent of the rewriter fixes.

### Phase C — FlareSolverr fallback (conditional on B + E being insufficient)

Model: **FlareSolverr is a Cloudflare-clearance-cookie vending machine, not a transport.** Never route per-page PNGs through it (3–5 s/call × 50 pages = UX disaster; FlareSolverr base64-wraps binary bodies; `/api/jmuse` needs custom `Authorization` headers we mint locally).

- New admin setting `flaresolverr_url` + optional `BROWSARR_FLARESOLVERR_URL` env var.
- New `src/sources/flaresolverr.rs` helper: `solve_for(url) -> Result<Vec<Cookie>>`. POSTs `{cmd: "request.get", url, session: "musescore"}`, returns `cf_clearance` / `__cf_bm` cookies.
- One long-lived session id, lazy-created.
- In `musescore.rs`: on first 403 (or specific Cloudflare body shape), call `solve_for`, inject cookies into our existing `Arc<Jar>`, retry once. Fast path stays plain HTTP.

**Known risk:** FlareSolverr's headless-Chrome JA3 differs from rustls's; cookies pass but Cloudflare can still flag the subsequent reqwest call. Rare in practice; worst case forces every call through FlareSolverr, at which point MuseScore becomes prohibitively slow and we reconsider the whole bet.

**Network note:** `sheet-music-browsarr` and the existing `flaresolverr` Portainer stack are on different Docker networks. Either move stacks together or use host IP `http://192.168.0.132:8191`.

### Phase F — Disk PDF cache + cap bump

- Cache path `/cache/musescore/{cache_version}/{id}-{bundle_hash_prefix}.pdf`. `cache_version` constant baked into `musescore.rs` (bump on rewriter changes). `bundle_hash_prefix` = first 8 chars of sha256 of the rewritten bundle (auto-invalidates on MuseScore deploys). Atomic `.tmp` + rename. Cache only successes.
- Separate `/cache` volume mount in `docker-compose.yml` (don't pollute `/config`, which is the small backed-up settings/DB volume).
- Bump `MAX_PDF_BYTES` (`src/routes/public.rs`) from 10 MB → 25 MB to match admin path. 50-page scores at 96 DPI land near the 10 MB cap; legitimate content needs headroom.
- Admin "purge MuseScore cache" button in `/admin/sources` — `walk_dir` + delete + audit-log entry. No selective purge in v1.

### Phase G — UX hardening

- Replace silent 302 in `pdf_handler` with an inline failure page (`templates/pdf_failed.html`): "Couldn't fetch this PDF directly. [Open on MuseScore →] [Back to search]". Same template usable for IMSLP/Mutopia failures.
- Public banner on `search_results.html` when any source's `SourceHealth.is_degraded()`. Auto-clears on next success.
- "Open PDF" on a degraded source's cards swaps to "Open on MuseScore" + direct `external_url` link (skip the doomed proxy attempt).

### Explicitly NOT in scope (per Plan-agent review)

- Persisting `SourceHealth` to SQLite — in-memory is enough; `audit_log` is the durable record.
- Per-source policy enum on the `Source` trait — keep it small; FlareSolverr stays implementation-private to `musescore.rs`.
- Parallelizing `/api/jmuse` calls — a `buffer_unordered(4)` would give a ~4× speedup; defer until pipeline is reliable.
- Failure cache — Cloudflare rate-limits clear themselves; we'd just retry on next user action.
- Pre-emptive FlareSolverr build — Phase C is conditional on B+E being insufficient.
- Selective per-score cache purge UI — bulk purge button only in v1.

---

## Deferred — potential extensions (not started)

These remain valuable on paper; revisit when audit-log evidence shows the missing catalog is worth the build-and-maintain cost. The plan stays here as a reference for the future you who wants to extend the app rather than spec it from scratch.

### Phase 4 — Torznab indexer support (admin-only initially) (target: 1 weekend)
- `/admin/indexers` CRUD
- `sources/torznab.rs` parallel fan-out + XML parse + merge
- Search results show source badge (IMSLP vs each tracker)
- Admin can trigger a torrent download → enters `pending` queue (workers don't act yet)

**Verify**: configure one Torznab feed, search blends results, admin queues a torrent, queue row appears.

### Phase 5 — Whatbox qBit + rsync queue worker (target: 1 weekend)
- `workers/queue.rs`, `workers/qbit.rs`, `workers/rsync.rs`
- State machine drives `pending → downloading → completed → rsyncing → done`
- SSH key generated, pubkey added to Whatbox, key path mounted into container
- `/admin/queue` page polls via HTMX `hx-trigger="every 5s"`
- Content validation: only PDFs (or PDF-only archives) move to library; everything else → `quarantined`

**Verify**: admin queues a torrent, watch states progress in real time, PDF lands at `/Volume1/media/sheet-music/`.

### Phase 6 — Public torrent triggers + moderation queue (target: 1 weekend)
- `/download` POST: Turnstile + rate limit + quota checks + content-validation pre-checks (max size from torrent metadata if available)
- If `public_downloads_require_approval = true`: state starts at `pending_approval`, admin clicks Approve in `/admin/queue` to release
- If `false`: goes straight into `pending`
- Storage-cap enforcement: refuse new downloads if library > cap
- Concurrent-torrent enforcement: queue beyond limit waits in `pending`

**Verify**: from an incognito browser, search → click download → Turnstile → either approval-pending or running; admin sees and approves; PDF arrives. Then exercise the limits: 6th request of the day = 429.

### Phase 7 — Polish (target: 1 weekend)
- Library page sortable/searchable + admin "hide from public" toggle
- Composer/instrument tagging — parse from PDF metadata where available
- `email_on_complete` for admin downloads
- Banned-IP middleware + admin UI for the audit log
- Dashboard with at-a-glance numbers (today's emails, downloads, library size, Whatbox quota headroom if exposed)

### Phase 8 (optional) — Wishlist / RSS-style watchers
- Saved searches that auto-grab new matches when they appear (admin-only feature, à la Sonarr)
- Cron-driven scanner over enabled indexers

---

## Setup checklist (one-time, during Phase 0–1)

- [ ] Pick `<your-domain>` and register Cloudflare hostname `music.<your-domain>`
- [ ] Create a GitHub repo `sheet-music-browsarr` and link a Personal Access Token with `write:packages` for ghcr.io
- [ ] Get a Cloudflare Turnstile site key + secret key (free) and put them in admin settings during Phase 2
- [ ] Generate `id_ed25519` on the NAS at `/Volume1/docker/sheet-music-browsarr/config/ssh/` (Phase 5)
- [ ] Add the pubkey to Whatbox's "Authorized Keys" page
- [ ] Create a Gmail (or other) app password for SMTP and add to admin settings (Phase 2)
- [ ] Decide an initial set of trackers (Torznab URLs + keys) for Phase 4

---

## Caveats worth surfacing

- **Copyright**: IMSLP is public-domain by design; tracker-sourced content is your judgment call. Sheet-music-browsarr won't filter on legality.
- **Whatbox AUP**: Review their policies for trackers used; programmatic torrent additions are fine in general.
- **`--remove-source-files` + seeding**: qBit may complain when source files are removed while seeding. Phase 5 may end up letting Whatbox keep files for a configurable seed period before rsync deletes them.
- **SMTP reputation**: even with the email quotas, sending to many arbitrary recipients from a personal SMTP account can land you in spam folders or trigger provider warnings. Realistic email volume on a personal sheet-music site is small — but worth monitoring.
- **Turnstile dependency**: sheet-music-browsarr depends on Cloudflare staying up. Acceptable since the whole site already depends on the tunnel.

---

## End-to-end verification (full system)

1. From a foreign IP, visit `music.<your-domain>` — search page loads in <500ms
2. Search "肖邦 夜曲" — results show Chopin Nocturnes (multilingual works)
3. Click "Email me" on a result, fill in any email + complete Turnstile — email arrives with PDF attached
4. Click "Download" on a tracker result, complete Turnstile — appears in `/admin/queue` (either pending or pending_approval)
5. Admin approves (if required), watches state progression, PDF lands in `/Volume1/media/sheet-music/`
6. Hammer `/email` from a script: 6th request from the same IP → 429, audit log has the row
7. `docker logs sheet-music-browsarr` — clean structured logs, no panics, no leaked secrets
