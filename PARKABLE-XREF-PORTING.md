# Parkable `sem` cross-repo tooling — porting guide (Rust fork → thin TS script)

This documents everything needed to reimplement the Parkable-specific `sem`
capabilities as a **thin Node/TS script driving stock tools**, instead of the
forked Rust binary. Move this file into the harness repo and build from it.

The canonical, tested implementation lives in this fork's Rust source — paths
are referenced throughout so you can check the nitty-gritty. The Rust tests are
the behavioural spec; port them as unit tests.

---

## 0. Why we're doing this

- The forked binary added: (1) Java/Kotlin scope-resolution fixes, (2) an async
  task-queue edge overlay, (3) cross-repo (`xref`) impact, (4) a slim build.
- **(1) is now merged upstream** → available from `brew` on the next `sem`
  release. So the fork's only remaining value is (2) and (3), which are
  **URL-string-keyed joins**: pure extraction + the within-repo call graph +
  matching logic. None of it needs to live inside the `sem` binary.
- Replacing the fork with a script removes: a committed 15 MB ARM-only binary,
  rebuild-on-upstream-release, and the rebase dance. Everyone has `brew` +
  `node`/`npx`, and we auto-benefit from upstream `sem` improvements.

### What we lose (accept this)
The fork injected synthetic edges into `sem`'s own graph, so async/cross-repo
links appeared **transparently** in `sem impact` / `sem context` / the MCP
tools. A script reconstructs links **per query, on demand**. For the questions
devs actually ask ("who enqueues this handler", "who consumes this endpoint")
on-demand is fine; you just lose silent inclusion in generic `sem_impact` calls.

---

## 1. Prerequisites (all stock)

| Tool | Role | Notes |
|---|---|---|
| `sem` (brew) | entity extraction + within-repo call graph | needs the release containing the merged Java/Kotlin fixes |
| `ripgrep` (`rg`) | route / URL text extraction | |
| `node` / `npx` | run the script | |

**Not needed:** the `tree-sitter` CLI. Our extraction is regex over source +
`sem`'s entity line-ranges — we never issue tree-sitter queries directly.

---

## 2. The two capabilities to port

- **A. Async task overlay** (within the API repo): a Cloud Tasks producer
  `TaskOptions.builder()…withUrl("/api/v1/tasks/…")` → the JAX-RS handler that
  serves that URL.
- **B. Cross-repo `xref`** (API ↔ TS clients): a client request URL (SWR key /
  key-builder / direct `get`/`post`/… call) → the JAX-RS handler, both
  directions, with within-repo transitive reach (affected UI / which endpoint).

Both are the same shape: extract a URL key on each side, **join on (path,
verb)**, then use `sem`'s call graph for transitive reach.

---

## 3. Parkable domain conventions the matching relies on

These are hardcoded assumptions — they're what make the join tractable.

**API (Java / JAX-RS, `jakarta.ws.rs`):**
- A resource class has a class-level `@Path("/api/vN/<thing>")` base.
- A handler method has a method-level `@Path("/sub/{id}")` (relative) **and** an
  HTTP verb annotation (`@GET`/`@POST`/`@PUT`/`@DELETE`/`@PATCH`).
- Full route = class base + method path, e.g. `/api/v2/parks/{id}`.
- **Versioned by class**: `ParksResource` → `/api/v1/parks`, `ParksResource2` →
  `/api/v2/parks`, `ParksResource3` → `/api/v3/parks`.
- Path params use `{name}`.

**Clients (TS, web `parkable-web-next` + mobile `parkable-mobile`):**
- axios `baseURL = ${config.api}/api/`; URLs are written **relative to it**,
  with no leading slash and no `api/`, e.g. `` `v2/parks/${parkId}` ``.
- ⇒ **server path = `/api/` + clientURL** (after stripping any `?query`).
- Reads use SWR: `useMySWR(key, fn = get)` — **GET by default**.
- Mutations are direct calls: `post`/`put`/`patch`/`del(url, body?)` — URL is the
  first arg. (Note: the delete wrapper is `del`, not `delete`.)
- Key-builders return the URL: `parkKey = (id) => `v2/parks/${id}?…``; hooks call
  the key-builder (`usePark` → `parkKey`). The URL literal lives in the
  key-builder, **not** the hook.
- Path interpolation uses `${expr}` template literals.
- Mobile source lives under `react/` (not `src/`) and uses `v1/...` URLs.

**Async tasks (API):**
- Producers enqueue with `TaskOptions.builder()…withUrl("<absolute /api path>")`,
  always a `.withUrl(...)` call (may be a literal or `"prefix/" + id` concat).
- Handlers are normal JAX-RS resources (class often named `…TasksResource`).

---

## 4. Stock-tool building blocks (commands + JSON shapes)

### `sem entities <file> --json`
Array of entities in one file:
```json
[ { "name": "retrievePark", "type": "method",
    "start_line": 137, "end_line": 183,
    "parent_id": "src/.../ParksResource2.java::class::ParksResource2" } ]
```
- Use for **entity identity + line ranges**, and to attribute a `rg` hit to its
  **innermost** entity (smallest `[start_line, end_line]` containing the hit
  line). Don't attribute to the class/file slice — it double-counts children.
- Entity id format is `<file>::<type>::<name>`, nested via the parent chain
  (e.g. `…::class::ParksResource2::method::retrievePark`). `parent_id` links a
  method to its class (needed to fetch the class's base `@Path`).

### `sem impact <entity> --json [--file <f>] [--dependents]`
```json
{ "dependencies": [ { "entityId": "src/.../parkKey", "file": "...",
                      "lines": [26,27], "name": "parkKey", "type": "function" } ],
  "dependents":   [ ... ] }
```
- `dependents` = who depends on it (callers). `dependencies` = what it uses.
- This is the **within-repo call graph**. Use:
  - `dependencies` to resolve a hook → its key-builder (the call site).
  - `dependents` to find affected UI (who uses a call site), transitively.
- Check `sem impact --help` for the exact **transitive / depth** flags in the
  stock release (the fork walked dependents to a bounded depth; you can either
  use sem's transitive mode or BFS over repeated `--dependents` calls yourself).
- `--file` disambiguates when a name exists in multiple files.

### `rg` for the URL/route text — see §5 for the exact patterns.

---

## 5. Extraction specs (verbatim regexes)

### 5.1 JAX-RS endpoints (server)
Reference: `crates/sem-core/src/parser/parkable_routes.rs::collect_handler_routes`.

- Class base `@Path` and method `@Path` (same regex):
  ```
  @Path\s*\(\s*(?:value\s*=\s*)?"([^"]*)"
  ```
- Verb (presence ⇒ the method is an endpoint; capture the verb):
  ```
  @(GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS)\b
  ```
- **Key finding (verify, don't assume):** in tree-sitter-java, annotations are a
  `modifiers` child of the declaration, so a `sem entities` method/class span
  **includes its leading annotations**. ⇒ the method's slice `[start_line,
  end_line]` already contains `@POST` / `@Path`; you do **not** need to scan
  lines above `start_line`. (Contrast Rust, where attributes are siblings.) The
  fork isolates the annotation header by taking lines up to the declaration line
  (`entity_header` / `line_declares`). Re-confirm with `sem entities` on a real
  resource before relying on it.
- Full route = `jaxrsSegments(classBase) ++ jaxrsSegments(methodPath)`.

### 5.2 Client calls (TS)
Reference: `crates/sem-core/src/parser/parkable_xref.rs::collect_client_calls`.

One regex over the file; iterate all matches:
```
(?:\b(get|post|put|patch|del)\s*(?:<[^>]*>)?\s*\(\s*)?[`"']\s*(?:/?(?:api/)?)(v\d+/[^`"'?\s]*)
```
- **group 1** = optional verb call wrapping the literal (handles `put<T>(`).
- **group 2** = the path: `v\d+/…`, stops at `?`/quote/space; an optional leading
  `/` and/or `api/` are consumed (not captured).
- Verb: group1 present → uppercase, mapping **`del` → `DELETE`**; absent → `GET`
  (the SWR default fetcher).
- Attribute each hit to its **innermost** entity (via `sem entities` line
  containment) → that's the call-site entity id.

### 5.3 Async producers (`.withUrl`)
Reference: `parkable_routes.rs::with_url_args` + `producer_segments`.

- Find `.withUrl(` then **paren-balance, string-aware**, to capture the full
  argument expression (a plain regex won't handle `"a/" + f(x) + "/b"`).
- From the arg, pull double-quoted **string fragments**; replace each dynamic run
  (`+ expr +`) with a wildcard sentinel; concat; strip `?query`; require it to
  start with `/` (literal-anchored). Bare `.withUrl(url)` (no literal) →
  unresolvable, skip.
- The exact char-walk is in `producer_segments` — port it; it's ~30 lines.

---

## 6. The join algorithm (the part to get exactly right)

This is where the fork had real bugs that only tests caught. Port the rules
**and** the tests. Reference: `parkable_routes.rs` (`Seg`, `to_segments`,
`jaxrs_segments`, `routes_match`) + `parkable_xref.rs` (`endpoint_matches_call`,
`best_endpoint`).

### 6.1 Segment model
- Split a path on `/`, drop empty segments.
- A segment is **`Wild`** if it's a JAX-RS template (`{…}`), a TS template
  (contains `{`, i.e. `${…}`), or a producer dynamic gap; else **`Lit(text)`**.
- Normalize client/producer URLs to the server space first: **prepend `/api/`**
  and **strip `?query`**.

### 6.2 Matching rule — ASYMMETRIC (the handler is the pattern)
```
match(handlerSegs, candidateSegs):
  if handlerSegs.length != candidateSegs.length: false
  for (h, c) of zip:
    h == Wild   -> ok (matches anything)
    h == Lit(a) -> require c == Lit(a)   # candidate must supply it literally
  -> all ok
```
> **Why asymmetric:** a candidate (client/producer) wildcard must **not** satisfy
> a handler **literal**. Symmetric "wildcard matches anything on either side"
> produces crossed-wildcard false positives — e.g. client
> `/api/.../{id}/apply-scheduled-price-changes` wrongly matched handler
> `/api/.../billingParkingSubscriptions/{id}` because each side had one wildcard
> in a different position. This was a real, observed bug. Keep the regression
> test (`crossed_wildcards_do_not_false_match`).

### 6.3 Verb
Match **path AND verb**. Critical: `v3/parks/${id}` is simultaneously a GET
(`usePark`), a PUT (`updatePark`), and a DELETE on the server. Path-only
matching wires a read hook to the delete endpoint. Tests:
`get_hook_matches_get_endpoint_not_put`, `put_call_matches_put_endpoint`.

### 6.4 Specificity tiebreak
If several handlers match a candidate, pick the one with the **most `Lit`
segments**. E.g. client `v3/parks/ids` matches both `/api/v3/parks/ids` and
`/api/v3/parks/{id}` (since `{id}` is Wild); prefer the all-literal one. Test:
`literal_collection_endpoint`.

### 6.5 Other normalization
- Versioned paths must not cross-match (`v2/...` vs `/api/v3/...`) — falls out of
  literal segment equality. Test: `versioned_paths_do_not_cross_match`.

---

## 7. Answering the queries (script behaviour)

Build per-repo manifests once: `endpoints[]` (from API repos) and
`clientCalls[]` (from TS repos), each with `{entityId, name, file, line, verb,
segs}`. Then:

### Direction A — target is an API handler (or an entity handlers depend on)
1. `endpoints` for the target =
   - if target is itself a handler → just it;
   - else → handlers that **transitively depend on** target
     (`sem impact <target> --dependents` transitively ∩ handler set). This gives
     "change a service method → which endpoints → which UI".
2. For each endpoint, for each **client** repo: client calls whose `(segs, verb)`
   match (§6) = the consumers.
3. For each consumer call-site: `sem impact <callSite> --dependents` (bounded
   depth) in that client repo = **affected UI**. Truncate for display; keep full
   for `--json`.

### Direction B — target is a client entity
1. Reachable call sites from target = target itself if it's a call site, **plus**
   call sites among target's transitive **dependencies**
   (`sem impact <target> --json` → walk `dependencies`). This is what resolves
   `usePark` → `parkKey` (the hook doesn't hold the URL; the key-builder does).
2. For each reached call site → `best_endpoint` across the API repos (§6.2–6.4).

### Async (within the API repo)
- "Who enqueues handler X?" → get X's full route; `rg` for `.withUrl(` containing
  the route's literal tail across the API repo; attribute to the producer entity.
- "What does producer P enqueue?" → extract P's `.withUrl(...)` (§5.3) → match
  against the endpoint manifest.

---

## 8. Edge cases & known limitations (carry these over verbatim)

- **POST-in-SWR-key**: `useMySWR(["v3/parks/ids", …], postFetcher)` — the URL is a
  string array element with no wrapping verb call ⇒ defaults to GET ⇒ won't match
  the POST handler. This is a **silent non-link, never a wrong link** (the safe
  failure). Note it; don't "fix" by defaulting to something riskier.
- **Non-literal URLs**: bare `.withUrl(url)` or a URL built from a non-string
  constant → unresolvable; skip and ideally log a count of skipped sites
  (silent misses read as "no caller", which is worse than a visible gap).
- **URL shape anchor**: only `v\d+/…` (optional leading `/api/`) is recognized.
  A client URL not in that versioned shape is missed.
- **Innermost-entity attribution** is required; attributing to the file/class
  slice double-counts every nested call.
- **Java annotation spans**: relied-upon behaviour (annotations inside the entity
  span) — re-verify against the stock `sem` release with `sem entities`.
- **Mobile layout**: source under `react/`, `v1/...` URLs — make repo roots /
  globs configurable, don't hardcode `src/`.
- **Tilde paths**: if the CLI accepts `tag=~/path`, expand `~`/`~/` yourself —
  the shell does **not** expand a tilde after `=` mid-word. (Ref:
  `parkable_xref::expand_tilde`.)

---

## 9. Canonical source map (the fork — your spec + reference)

`crates/sem-core/src/parser/parkable_routes.rs`
- `collect_handler_routes` — JAX-RS endpoint manifest (class+method `@Path`, verb).
- `Seg`, `to_segments`, `jaxrs_segments` — segment model.
- `routes_match` — **the asymmetric matcher** (§6.2).
- `with_url_args`, `producer_segments` — `.withUrl` paren-balance + dynamic→wild.
- `entity_header`, `entity_slice`, `line_declares` — annotation/header isolation.
- `#[cfg(test)] mod tests` — **port these as your unit tests.**

`crates/sem-core/src/parser/parkable_xref.rs`
- `collect_endpoints`, `collect_client_calls` — manifests; client URL regex; verb
  rules; innermost-entity attribution.
- `endpoint_matches_call`, `best_endpoint` — verb+path match + specificity.
- `build_report` / `api_to_clients` / `client_to_api` / `bfs` / `matches_query` /
  `expand_tilde` — the orchestration both directions (and the `XrefReport`
  JSON shape, if you want output parity).
- `#[cfg(test)] mod tests` — verb disambiguation, crossed-wildcard, versioned
  paths, collection endpoint, regex extraction. **Port these.**

---

## 10. Suggested script shape

```
npx tsx sem-xref.ts <entity> --repo api=/abs/parkableapi --repo web=/abs/web \
                              [--repo mobile=/abs/mobile] [--file f] [--depth n] [--json]
```
Modules:
- `extract.ts` — `rg` + `sem entities --json` → `endpoints[]`, `clientCalls[]`,
  `producers[]` per repo. (Cache per run.)
- `match.ts` — `Seg`, `toSegments`, `routesMatch`, `bestEndpoint` — **pure,
  unit-tested** (port the Rust tests 1:1).
- `graph.ts` — wrappers over `sem impact … --json` (dependents/dependencies,
  bounded).
- `report.ts` — assemble + format (terminal + `--json`; reuse `XrefReport`
  shape).
- `async.ts` — the within-API `.withUrl` ↔ handler queries (§7).

Skill (harness): a markdown skill that says *when* to reach for this (cross-repo
"who consumes / what does this call" questions; async "who enqueues"), how to
pass repo paths, and how to read the output — and that for anything outside the
script's shape, Claude can run `sem`/`rg` ad hoc using the rules above.

Perf: dominated by `sem` parsing each repo (~seconds for the API + one client).
Build manifests once per invocation and reuse across both directions.

---

## 11. Decision recap

Keep the **fork binary only if** you specifically need the async/cross-repo edges
to appear *transparently inside the `sem` MCP server* (so a generic `sem_impact`
silently includes them). That transparent-graph integration is the one thing the
script cannot reproduce. For everything else — deterministic answers, brew-based
deploy, zero binary in git, auto-upgrade with upstream `sem` — the script wins.
