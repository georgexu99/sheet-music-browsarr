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

### Phase 3 — Multilingual search (target: 1 weekend)
- `i18n/` module + alias CSV + OpenCC + jieba + pinyin crates wired
- Search pipeline expands query → queries IMSLP with each variant in parallel → merges
- Tests: `xiaobang`, `肖邦`, `蕭邦`, `Chopin` all return Chopin results

**Verify**: each of the four query forms produces non-empty, overlapping results.

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
