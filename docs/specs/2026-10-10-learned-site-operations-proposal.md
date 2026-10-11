# Learned Site Operations — Proposal

**Date:** 2026-10-10
**Status:** PROPOSED. Awaiting operator yes/no on the questions in §13. It authorizes nothing. No code, tool, crate, dependency, or stored credential exists yet.
**Baseline:** `origin/main` at `1bd1bc37`.
**ROADMAP item:** proposed new item under "Ocean Browser (OceanWebKit)": *Learned site operations: teach a site once, replay it over plain HTTP*.
**Prior art:** [`goodnight000/api-anything`](https://github.com/goodnight000/api-anything) (MIT, TypeScript, created 2026-09-27, read at `fe5cca7`). This proposal reimplements its ideas in Rust. It does not vendor its code.

## In plain words

Agents waste time and money driving a browser to read the same kinds of pages over and over.
A better way: watch the page once while it asks its own server for data, save that request as
a reusable recipe, and from then on send the recipe straight to the server with new inputs.
The answer comes back as JSON in under a second. No browser and no model tokens are involved.

This proposal adds that ability to Ocean as **learned site operations**. The plan has three parts:

1. **An engine** (a Rust library and a command-line tool) that stores recipes, replays them, pulls
   out the fields we want, tells the caller clearly when something broke, and repairs a recipe
   when the site changes, but only after proving the repaired recipe works.
2. **A learner** that builds recipes from a recording of browser traffic. The recording can come
   from any browser's standard export (HAR) today. It can come from Ocean's interim Chromium
   backend if the operator approves one new tool. Later it comes from OceanWebKit's network capture.
3. **Two first users.** Creator research gets real TikTok view counts with no agent fan-out. The
   North Star fan wall gets missing Instagram/TikTok details, but only within that product's rules.

It does not change the 19 browser tools, `web_fetch`, or the OceanWebKit program. It does not
log in to anyone's account in v1, and it never sends write requests in v1.

## Decision requested

Accept this proposal's architecture (§4) and authorize **Phases 1–3** (§9): the engine, the
HAR-based learner, and the Ocean plugin, plus the creator-research consumer. Phases 4–6 each need
their own yes. Phase 4 adds a browser tool, Phase 5 touches login cookies, and Phase 6 depends on
OceanWebKit.

## 1. What the prior art does, and what we take

API Anything is about 7,400 lines of TypeScript code and about 14,000 lines of tests, from one
author, two weeks old. The core loop:

- **Learn.** Headless Chrome loads a trigger URL twice, with two different example inputs. Every
  request is captured. The request whose leaves track the example inputs is picked by an evidence
  score, not by timing. Its leaves become **slots** wherever the example value shows up, through
  nested encodings such as JSON inside a form field inside a percent-encoded string. Cookies and
  tokens become **references** to a local session store, never values. A save-time secret scan
  fails closed.
- **Call.** The slots are filled and the request is sent with plain `fetch` (tier 1). If that is
  blocked, the fetch runs inside a Chrome page (tier 2). As a last resort for reads, the page
  itself runs and its own response is read (tier 3), but only when the trigger ran with *this
  call's* arguments.
- **Extract.** A JSON path with `[*]` flattening, renaming `pick`s, a regex for JSON embedded in
  HTML, CSS selectors for HTML lists, an output cap, and a deterministic **outline** that shows a
  scout what a response contains without reading it all.
- **Classify.** Every failure is one of `drift | auth | rate | blocked | input | error`, each with
  a one-step `next` hint.
- **Repair.** First a browser-free **rescan**: fetch the trigger page and up to 40 of its scripts,
  and find a rotated id of the recorded shape near its recorded anchor. Then, if needed, a
  **recapture**. A repaired recipe is saved only after a replay returns `ok` with the extract
  resolving. It is never saved if any parameter lost its slot or changed encoding.
- **Interfaces.** An MCP server with four tools (`list_sites`, `list_operations`,
  `call_operation`, `login`), a CLI, and a TypeScript library. MCP can only *call* operations.
  Creating them needs a shell plus a skill.

**We take:** the spec model (slots, refs, volatiles, match, response recipe), the byte-preserving
layered codec, the learner's rule set, extraction and outline, the error classes and `next`
hints, the repair order and its "save only after a proven replay" rule, and the tests' adversarial
fixture design.

**We leave or defer:** Playwright (Ocean has its own browser program); writes and the
write-interception guard (rejected for v1, §10); tiers 2–3 (deferred to OceanWebKit, Phase 6);
import of cookies from the operator's personal browser profiles (Phase 5, operator-gated); and
shelling out to `security` for Keychain access.

## 2. Constraints this design must respect

These are current Ocean rules. The design is shaped around them, not exceptions to them.

1. **Browser program.** [`2026-07-19-ocean-webkit-browser-program.md`](2026-07-19-ocean-webkit-browser-program.md) is
   ratified. Chromium is quarantined behind the default-off `legacy-chromium` feature, and default
   builds must never launch or probe a browser. Network capture parity belongs to OceanWebKit's
   NetworkProcess. Partial-fidelity capture must not ship as parity. Models never receive raw CDP.
   As of this baseline, the program's first hard checkpoint (ROADMAP "Ocean Browser") is unchecked.
2. **Browser tool pin.** The 19 `browser_*` tool schemas are pinned by `EXPECTED_TOOLS` in
   `crates/ocean-runtime/src/tools/browser/mod.rs`. Adding a tool is a separate, explicit decision.
3. **Extension ownership.** [`2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](2026-07-14-ocean-extensions-architecture-and-migration-manifest.md)
   says integrations, tool plugins, and external-service credentials are extension-owned. Core owns
   permissions, secret authority, and capability registration. Plugin tools are always
   permission-gated as `plugin__<plugin>__<tool>` ([`../PLUGINS.md`](../PLUGINS.md)).
4. **No egress policy exists in core.** No domain allowlist, no SSRF guard. `web_fetch` is
   ungated, GET-only, with no cookies. So this feature brings its own egress guard, and it does
   **not** extend `web_fetch`.
5. **Secrets.** Only `<scheme>:<key>` references cross into extensions. No crate uses the macOS
   Keychain today, and the `keychain:` resolver is deferred. Provider credentials in `auth.json`
   are out of bounds for this feature.
6. **Documentation contract.** Built, pending, and rejected scope stay separate (§10). Never record
   credentials or token locations.

## 3. Where it fits — the short version

```text
                ┌─────────────── capture sources (pluggable) ───────────────┐
                │ A. HAR file exported by any browser        (Phase 2, now) │
                │ B. legacy-chromium recorder tool  (Phase 4, operator yes) │
                │ C. OceanWebKit Network domain    (Phase 6, when it ships) │
                └───────────────────────────┬───────────────────────────────┘
                                            │ Vec<Exchange>  (HAR 1.2 shaped)
                                            ▼
┌──────────────────────── ocean-siteops (engine library) ─────────────────────────┐
│ codec · spec · learner · extract/outline · classify · replay (tier 1) · repair  │
│ store (<config>/siteops/) · session refs · egress guard · pacing                │
└──────────────┬───────────────────────────────┬──────────────────────────────────┘
               │                               │
   plugin__siteops__* tools          `ocean-siteops` CLI (no model)
   (permission-gated, in Ocean)      scripts, cron, batch JSONL, verify
               │                               │
        Ocean agents                creator-research engine · fan-wall sync
```

The key move is making the learner **consume a neutral recording format instead of driving a
browser**. That separates the useful part, turning traffic into a template, from the part Ocean
has deliberately put on hold, which is owning a capture-grade browser. The engine works the day
it lands, using HAR exports from the operator's own Chrome, Safari, or Firefox. Each capture
source can be swapped in later without touching the engine.

## 4. Architecture

### 4.1 Packaging

- **`crates/ocean-siteops`.** A new **non-default** workspace member containing a library and one
  binary, `ocean-siteops`.
  - It depends on no other Ocean crate: not the runtime, the daemon, or the browser. The daemon
    does not link it, which keeps it outside core.
  - It needs a `crates/AGENTS.md` row and a non-default rationale, as `cargo xtask docs-check` enforces.
- **`plugins/ocean-siteops/`.** Holds `plugin.toml`, `AGENTS.md`, `README.md`, and an installer.
  The binary's `plugin` subcommand speaks the existing newline-delimited JSON-RPC plugin ABI
  ([`../PLUGINS.md`](../PLUGINS.md) §2).
- **The skill.** `plugins/ocean-siteops/skill/SKILL.md` teaches an agent the scout → propose →
  learn → verify loop.
- **Dependencies, new to the workspace, behind `cargo deny`:**

  | Phase | Crates |
  |---|---|
  | Phase 1 | `scraper` (HTML selectors), `psl` or `publicsuffix` (registrable domain) |
  | Phase 5 | `aes`, `cbc`, `pbkdf2`, and `security-framework` on macOS only |

  Already present: `reqwest` 0.12 (we build our own jar, so the `cookies` feature is not needed),
  `serde_json`, `regex`, `url`, `base64`, `sha2`, `fs2`, and `rusqlite`, which is Phase 5 only.
  MSRV stays 1.88.

### 4.2 Engine modules

Sizes are planned Rust lines of code, excluding tests.

| Module | Job | Size |
|---|---|---|
| `codec` | Walks every leaf of a request: path segments, query pairs (repeats as `query[n]:k`), headers, form pairs, JSON body, JSON-in-string, base64(url)-JSON that round-trips. Rewrites a leaf by **splicing bytes**, re-encoding only the layers the slot touches, so key order, whitespace, big integers, and the site's own percent-encoding style survive. `serde_json` cannot do this; it needs a span scanner. | ~650 |
| `spec` | The `Site` / `Operation` / `Slot` / `Volatile` / `Match` / `Response` types, the step grammar (`path:i`, `query[n]:k`, `form[n]:k`, `header:n`, `json:<RFC6901>`, `b64`, `body`), validation, and versioning. **Import** of API Anything `sites/*.json`, so its bundled read operations (22 across 9 sites, per its README) are usable on day one as data. | ~400 |
| `exchange` | The neutral recording type, plus a HAR 1.2 reader with bounded body sizes. It also reads `postData`, base64 bodies, and `_resourceType`. | ~350 |
| `learn` | Candidate ranking (evidence score, noise filters, echo exclusion), slot discovery, templated slots, escape inference, session-ref and credential rules, volatile anchors, match building, two-run diff (nonce → `min_tier`), "disproved slot" re-learn, and response-recipe suggestion. Ported rule by rule with its edge tests. | ~2,200 |
| `extract` | Path with `[*]`, `pick` (`name=path`, `name=path~regex`), `embedded` (regex anchor + JSON value end), `html` (`items` + `fields` with `@attr`), XSSI prefix strip, shape inference, output cap. | ~650 |
| `outline` | A deterministic scout summary: where the example appears, suggested extract and picks, JSON-LD, `__NEXT_DATA__`, `window.X=` state, ready-to-use `embedded`/`html` recipes. | ~350 |
| `classify` | Maps a response to `ok / drift / auth / rate / blocked / input / error`, plus a `next` hint (login walls, challenge pages, 429/Retry-After, empty data with valid shape). | ~450 |
| `replay` | Tier-1 client: `reqwest` with redirects off and a hand-rolled redirect policy (credentials stripped across origins), its own cookie jar keyed by the public-suffix list, gzip/br/zstd, pacing, timeouts, and a size cap. | ~700 |
| `egress` | Resolves, then refuses loopback, private, link-local, and metadata addresses, pinning the resolved IP into the connection. Request host must match the operation's `match.host`. A fixture-only `--allow-local` exists for tests. | ~200 |
| `repair` | `onDrift` order (example replay → `input` vs heal), browser-free **rescan** (trigger page + ≤40 scripts, anchor-nearest token of the recorded shape, ties refused), candidate validation by replay, loop guards (10-min re-heal window, 30-min stale mark), `heals.jsonl`. Recapture plugs in once a capture source can be driven (Phase 4/6). | ~800 |
| `store` | `<config>/siteops/` layout (§4.3), atomic writes, an `O_EXCL` lock, 0600 files / 0700 directories, capture pruning, remembered tiers. | ~300 |
| `secrets` | Credential-name and entropy rules, a save-time scan through ≤3 encoding layers, fails closed. | ~250 |
| `cli` + `plugin` | `sites`, `ops`, `call` (incl. `--batch` JSONL in/out), `outline`, `learn`, `verify`, `heal`, `export`, and `plugin` (JSON-RPC stdio). | ~900 |

The Phase 1–3 total is about 8,000 lines of Rust plus tests (§11). The test suite should be at
least as large as the code, as it is upstream.

### 4.3 Storage

The config directory is the one Ocean already uses (`~/.config/ocean-rs` by default).

```text
<config>/siteops/
  sites/<site>.json            # specs: templates, refs (never values), examples, minTier
  sessions/<site>.json         # 0600; cookie and token VALUES; empty in v1 (logged-out only)
  captures/                    # bounded: newest 20, none older than 24h; redacted
  heals.jsonl                  # append-only repair log: op, old/new template hash, proof call
  state.json                   # remembered tiers, stale marks, per-site last-call times
```

Specs are plain JSON so they can be reviewed, diffed, and committed to a consumer repo. A spec
that contains a live secret cannot be saved (`secrets`).

### 4.4 Agent tools (the plugin)

These are exposed as `plugin__siteops__<tool>` and always permission-gated by the runtime.

| Tool | Input | Output |
|---|---|---|
| `sites` | — | known sites, op counts, last verify result |
| `ops` | `site` | each op's params (type, example, hint), caveats, `minTier`, read-only flag |
| `call` | `site`, `op`, `args` | `{ok, class, tier, data, ms, source_url, fetched_at}` or `{ok:false, class, reason, next}`. Record lists come back as `{columns, rows}` to save tokens. |
| `outline` | `har_path` (or a capture id), `example` | the scout summary (§4.2), never full bodies |
| `learn` | `site`, `op`, `har_path`, `har2_path`, `examples`, `examples2`, optional `match` / `pick_request` / `response` recipe | the saved op plus a proof call using inputs that were not examples |
| `verify` | `site` (optional) | per-op pass/fail with class |

Every `call` result carries **`source_url` and `fetched_at`**. That is what creator research's
"real number + source, or null, never guess" rule needs (§7.1). Nulls are never turned into zeros.

### 4.5 Capture sources

- **A. HAR import (Phase 2).** The operator or agent opens the page in any browser, exports
  "Save all as HAR" once per example input, and hands over the two files.
  - It has zero coupling to Ocean's browser and works today.
  - The plugin redacts `Cookie`, `Set-Cookie`, and `Authorization` values into the session store
    on read, then deletes or prunes the raw HAR.
- **B. Legacy recorder (Phase 4, needs a yes).** A new gated tool, behind `legacy-chromium`, that
  records one navigation's exchanges to a bounded, redacted HAR in `captures/`.
  - It subscribes to `Network.requestWillBeSent`, `requestWillBeSentExtraInfo`,
    `responseReceived`, `responseReceivedExtraInfo`, and `loadingFinished`.
  - It correlates them by `requestId` and fetches bodies eagerly at `loadingFinished`, because
    Chrome evicts them later.
  - Body storage is bounded per item and in total. It covers popup targets.
  - It is labeled **interim, not parity**, per the browser program.
  - Today's `netcap.rs` captures only URL, status, and MIME type, so it is not enough.
- **C. OceanWebKit (Phase 6).** The program's Network/Fetch/Storage parity and "Request-ID-correlated
  bounded body storage" gates are exactly what the learner needs. When that host ships, source B is
  retired. The same recorder contract (a HAR out) applies, so the engine does not change.

### 4.6 Tiers in Ocean

- **Tier 1 (plain HTTP)** is v1 and the only tier the engine runs by itself.
- **Tiers 2 and 3 need a browser page.** In v1 a call that needs one returns
  `class: "blocked", next: "needs_browser"`. The engine **never** launches Chrome itself, because
  that would get around the quarantine. Phase 6 adds a browser executor interface, implemented by
  OceanWebKit.
- **Known limit, same as upstream.** `reqwest` + rustls does not present Chrome's TLS fingerprint,
  so sites that fingerprint TLS (some Cloudflare and TikTok paths) may answer `blocked` at tier 1.
  This is not something to work around. It is a reason those operations wait for a browser tier.

## 5. Safety and security invariants

These hold in every phase. Breaking any of them is a stop condition (§12).

1. **Read-only.** v1 has no write operations at all. A captured request with a non-GET/HEAD method
   that changes state is refused at `learn` time unless it is a known read-shaped POST (GraphQL
   query, `batchexecute` read). Those are flagged in the spec and reviewed.
2. **References, not values.** Specs hold `cookie:NAME` / `session:op/NAME` references only.
   Values live in `sessions/`, 0600. The secret scan fails closed.
3. **Egress guard.** Public addresses only. The host is pinned to the operation's `match.host`.
   No redirects to other registrable domains carry credentials.
4. **No other caller's data.** Any browser-tier fallback (Phase 6) uses the site's own response only
   when every declared slot in the observed request equals the request built from *this call's*
   arguments.
5. **Repair is proven before it is saved.** A replay must return `ok` with the extract resolving,
   and the parameter contract must be unchanged. Otherwise the call returns an error, never stale or
   foreign data.
6. **Pacing.** At most one call per second per site by default, enforced across processes through
   `state.json` under the lock. Per-op overrides may only be *slower*.
7. **Logged-out by default.** v1 has no login flow and no cookie import (§13 Q4). Every bundled or
   learned op must work with an empty session.
8. **No raw CDP or HAR to models.** Agents see `outline` summaries and extracted fields, not raw
   traffic. Recordings stay in `captures/` and are pruned.
9. **Terms of service.** Each site file carries a `terms_note` written at learn time. Consumers own
   compliance. Nothing here is approved for a hosted customer-facing product (§7.2).

## 6. Spec format

The format is a superset of API Anything's, so its bundled specs import unchanged. The additions
are `terms_note`, `source` (`har` | `legacy-recorder` | `webkit` | `import:api-anything@<sha>`),
`learned_by` (session or agent id), and a `verify` block (last result and timestamp).

Here is an abbreviated example, one of the upstream bundled ops:

```json
{"name":"getProfile","readOnly":true,"minTier":1,"learnedLoggedIn":false,
 "request":{"method":"GET","url":"https://www.instagram.com/{username}/","headers":{"accept":"text/html"}},
 "slots":[{"param":"username","at":["path:0"]}],"volatile":[],
 "trigger":{"url":"https://www.instagram.com/{username}/"},
 "match":{"method":"GET","host":"www.instagram.com","path":"/*"},
 "response":{"format":"embedded","embedded":{"regex":"\"xig_user_by_username\":(\\{)\"pk\":\"\\d+\""}},
 "params":[{"name":"username","type":"string","required":true,"example":"nasa"}],
 "terms_note":"public profile page, logged out", "source":"import:api-anything@fe5cca7"}
```

## 7. First consumers

### 7.1 Creator research (TikTok median views)

**Today:**
- The engine lives at `ecfromthedc/creator-research-agent` and is local-only. Its Ocean agent
  package is not on `ocean-agents` `origin/main`.
- It needs one number per creator: the median or average views of recent videos, with a source
  URL. That is 327 locked TikTok handles for one campaign.
- It gets the number by fanning out 33 Sonnet agents that web-search aggregator sites. One run died
  at 25 of 33, and coverage stalled at **155 of 327 real numbers**. The rest are follower-based
  estimates, labeled as such.

**With learned operations:**
1. **Feasibility first (Phase 3, step 1).** Find which source answers at tier 1, logged out. There
   are three candidates, tested in this order:
   - (a) TikTok's own profile page: embedded rehydration JSON. Likely followers and likes only,
     not per-video views.
   - (b) TikTok's item-list API. Likely signed, which means it needs a browser tier.
   - (c) The aggregator pages the agents already used most: tikbuddy, thesocialcat, urlebird.
     Their HTML lists recent videos with views, so they fit `html` recipes.

   Each candidate is graded on tier-1 success over 20 handles that were not examples. **If none
   passes, the consumer stops here** and the result is recorded. That is an acceptable outcome;
   the engine still has value for other sites.
2. Replace the workflow fan-out with one deterministic batch run:
   `ocean-siteops call <site> <op> --batch handles.jsonl > views.jsonl`.
   - Paced, no model, re-runnable.
   - Each line is `{handle, median_views|null, source_url, fetched_at, class}`.
   - The median is computed from the returned per-video views over the stated window, never
     estimated.
3. `ingest_views.py` already reads `{results:[{handle, median_views, source}]}` from the workflow, so
   it consumes this with a small mapping (wrap the lines, rename `source_url`). The basis stays `scraped`, which keeps the observed > scraped > estimate
   precedence unchanged.

**Success measure:** real-number coverage above 155 of 327, zero model tokens per refresh, and a
full refresh in under 10 minutes at 1 call per second.

### 7.2 North Star fan wall (missing Instagram/TikTok details)

**Today:**
- `pulse-signal` sources fan content from seeds, yt-dlp (YouTube), and Firecrawl search.
- Firecrawl items have no thumbnail, views, author, or date. About a quarter of the wall never
  gets a date, because yt-dlp needs a login for IG/TikTok.
- It syncs daily, locally, and page loads never call external services.

**That repo's rules bind this consumer:**
- Session-based IG/TikTok scraping is **manual-only, never in the unattended daily pipeline**.
- Read-only, no media downloads, about 40 outbound calls a day.
- Every write goes through `Store::upsert_fan_content` (which honours takedowns).
- NULL means "not reported", never zero.
- New outbound vendors need an `approvals/` row.
- Fan content is off for hosted beta artists.

**So the plan is narrow:**
1. **Prefer official endpoints first.** TikTok's public oEmbed (thumbnail, title, author for a
   known video URL) needs no scraping at all. Instagram oEmbed goes through the Meta app once app
   review clears.
2. **Learned ops only for what oEmbed lacks,** logged out, on the operator's machine, inside the
   existing daily cap. They would fill views and post date for already-known URLs. They never
   discover new people.
3. **Thumbnail caveat.** Instagram CDN thumbnails are signed and expire. The wall must re-resolve
   them each sync or omit them, and must not hotlink a stale URL. Check whether TikTok oEmbed
   thumbnails expire too before relying on them.
4. **Hosted product: not authorized by this proposal.** It needs an `approvals/` row and a product
   decision in North Star, recorded there.

The North Star work lands in that repository. This proposal only provides the engine and states
the contract.

### 7.3 Not a consumer: Tides Net

Tides Net ranks an artist's own posts by views, likes, comments, and shares. The upstream logged-out
Instagram op returns no engagement counts, and TikTok item lists are likely signed. Apify stays
until a learned op proves the same fields at tier 1. Re-evaluate after the §7.1 feasibility step.

## 8. Relationship to OceanWebKit

This proposal **adds demand for**, and does not compete with, the browser program's §7 gates:
"complete network capture with bounded response-body retention" and "request-ID-correlated bounded
body storage." Learned operations are a concrete first client for those gates. When OceanWebKit
reaches Network/Fetch/Runtime parity, Phase 6 implements the engine's `BrowserExecutor` trait
(navigate trigger, capture to HAR, in-page fetch, read own response) against it. The legacy recorder
(Phase 4), if built, is then removed.

## 9. Phases

Each phase lands as its own reviewed PR with an `events.md` entry and the gates in §12.

| Phase | Scope | Needs |
|---|---|---|
| **0** | This proposal; ROADMAP item; docs index entry. | merge (docs-only) |
| **1 — Engine** | `codec`, `spec` (+ upstream import), `exchange` (HAR reader), `extract`, `outline`, `classify`, `replay` (tier 1), `egress`, `store`, `secrets`; CLI `sites/ops/call/--batch/verify/export`; axum **fixture server** reproducing upstream's adversarial cases (rotating persisted GraphQL ids in a hashed bundle, Google-style layered form/JSON with XSSI, login wall as 200 HTML and as redirect, rate limits, HTML lists, SPA). | yes on §13 Q1 |
| **2 — Learner** | `learn` from two HARs + examples; one-run learn for long examples; volatile anchors; two-run diff; disproved-slot re-learn; browser-free **rescan** repair; `heals.jsonl`. Ported edge tests. | Phase 1 merged |
| **3 — Plugin + first consumer** | `plugin` subcommand + `plugins/ocean-siteops/` + skill; creator-research feasibility step and, if it passes, the batch consumer (§7.1). Fan-wall contract handed to North Star (§7.2). | Phase 2 merged |
| **4 — Legacy recorder** | One new gated `legacy-chromium` tool that records a navigation to a bounded, redacted HAR; recapture repair wired to it. Labeled interim. | yes on §13 Q3 |
| **5 — Logged-in sites** | `login` that imports cookies **from Ocean's own Chrome profile** first; personal-profile import (Chromium AES-CBC + Keychain via `security-framework`, Firefox plaintext) only with a per-site operator confirmation. | yes on §13 Q4 |
| **6 — Browser tiers** | `BrowserExecutor` on OceanWebKit: tier 2/3, recapture without HAR. Retire Phase 4. | OceanWebKit Network/Runtime parity |

## 10. Scope ledger

- **Built:** nothing.
- **Pending (this proposal):** Phases 0–3 if accepted. Phases 4–6 each need a separate yes.
- **Rejected for v1:** write operations and the write-interception guard; CAPTCHA or challenge
  solving; TLS-fingerprint spoofing; the engine launching any browser itself; extending `web_fetch`;
  changing any of the 19 browser tool schemas; generic re-export through the `ocean-mcp` server; any
  use in a hosted customer product.

## 11. Effort estimate

The sizes come from a module-by-module read of the upstream source (§4.2).

| Slice | Rust code | Tests | One focused engineer | Ocean agent lanes, with review gates |
|---|---|---|---|---|
| Phase 1 engine | ~3,500 | ~4,000 | 2–3 weeks | 3–5 days |
| Phase 2 learner | ~3,000 | ~4,000 | 3–4 weeks | 1–1.5 weeks |
| Phase 3 plugin + consumer | ~900 | ~800 | ~1 week | 2–3 days |
| **Useful v1 (Phases 1–3)** | **~7,500** | **~9,000** | **6–8 weeks** | **~2–3 weeks** |
| Phase 4 recorder | ~800 | ~600 | 1–1.5 weeks | 3–4 days |
| Phase 5 logins | ~600 | ~600 | ~1 week | 2–3 days |
| Phase 6 browser tiers | ~1,500 | ~1,500 | 2–4 weeks | blocked on OceanWebKit |
| **Full upstream parity** | **~10,000** | **~12,000** | **~3 months** | **~5–6 weeks + OceanWebKit** |

Rust needs more code than TypeScript in two places. The codec does span splicing that `serde_json`
cannot. The browser layer (Phase 4/6) has to rebuild what Playwright provides for free. The
**riskiest** part is the learner's rules, roughly 1,500 dense upstream lines tuned against real
sites. It must be ported rule by rule *with* its edge tests, not reinvented.

## 12. Gates and stop conditions

**Per PR:**
- `cargo xtask docs-check`
- `cargo test -p ocean-siteops`
- `cargo clippy -p ocean-siteops -- -D warnings`
- `cargo deny check`
- the full `cargo xtask ci` before merge
- a fresh reviewer acknowledgement for Phases 1, 2, 4, and 5, because they involve security
  (egress, secrets, cookies)
- Phase 4 also runs the existing `legacy-chromium` clippy lane, plus an updated `EXPECTED_TOOLS`
  under its own approval.

**Mechanical guards, added in Phase 1 and kept:**
1. A test that walks every spec under `sites/` and fails if any contains a value matching the
   secret rules.
2. An egress test proving loopback, RFC1918, link-local, and `169.254.169.254` are refused without
   `--allow-local`.
3. A test proving no code path in `ocean-siteops` can spawn a browser or open a CDP socket. It
   greps the dependency graph for `chromiumoxide` and the source for `--remote-debugging`.
4. A test that a repair candidate failing replay leaves the saved spec byte-identical.

**Stop and report if:** an invariant in §5 would have to bend; a consumer needs a logged-in session
before Phase 5 is approved; a site needs tier 2/3 before Phase 6; or the creator-research
feasibility step fails all three candidates.

## 13. Questions for the operator

1. **Q1.** Accept §4's architecture and authorize Phases 1–3? *(Recommended: yes.)*
2. **Q2.** Ship the upstream bundled read ops as importable data, with MIT credit in `NOTICE.md`
   and `CREDITS.md`, or start with an empty catalogue? *(Recommended: import, verified on import.)*
3. **Q3.** Add the Phase 4 legacy recorder tool now, or rely on HAR exports until OceanWebKit?
   *(Recommended: HAR only until a consumer is blocked by it.)*
4. **Q4.** May Phase 5 import cookies, and from which profiles: Ocean's own Chrome profile only, or
   also personal browser profiles with a per-site confirmation? *(Recommended: Ocean's profile only,
   and no Instagram/TikTok accounts.)*
5. **Q5.** Before Phase 1 code, run a **one-day feasibility spike** for §7.1 using the upstream tool
   itself as a throwaway scout on the operator's machine (Node, not shipped, deleted afterwards)?
   This tests the costliest unknown, whether TikTok views answer at tier 1, before spending weeks.
   *(Recommended: yes.)*
