# Changelog

All notable changes to sem are documented in this file.

## [Unreleased]

### Added

- Parkable fork: `grammar-parkable` slim build (Java + TypeScript + JavaScript only). `sem-cli`/`sem-mcp` now take `sem-core` with `default-features = false` and forward a `grammar-*` group, so `cargo build --release -p sem-cli --no-default-features --features grammar-parkable` drops the 25 unused tree-sitter grammars and shrinks the binary from ~74 MB to ~14.5 MB. Default build is unchanged (`grammar-all`).
- Parkable fork: `sem xref` — cross-repo impact across the API and its TS clients (web panel, mobile). Pass repos as `--repo tag=path`. Joins client request URLs (SWR keys, key-builders, and direct `get`/`post`/`put`/`patch`/`del` calls) to JAX-RS handler routes on **path + HTTP verb** (`/api/` base prefix; `${x}`/`{id}` templates; versioned resource classes). Two directions: an API endpoint (or an entity endpoints depend on) → the client call sites that consume it plus their in-client dependents (affected UI); a client hook/component → the API endpoint(s) it transitively calls. Manifest-join over per-repo graphs (no merged graph); shares route extraction with the task overlay (`parser::parkable_xref`, `collect_handler_routes`). Terminal + `--json`.
- Parkable fork: link GCP Cloud Tasks producers to their JAX-RS handlers. A post-build overlay (`parser::parkable_routes`) matches `TaskOptions...withUrl(<url>)` producer call sites against `@Path` + HTTP-verb handler routes (composing class base path + method path, with `{id}` templates and `"prefix/" + id` concatenation handled), then emits synthetic `Calls` edges so the async hop is traversable. Wired into `sem impact`, `sem context`, and the MCP server (`sem_impact`/`sem_context`); in the MCP server the overlay is applied in-memory after the on-disk cache save so the persisted cache stays pure upstream data. `sem log` is git-history-based and has no relationship graph, so it is unaffected. Kept out of the upstream extraction pipeline to minimise merge conflicts.
- Start tracking project changes in `CHANGELOG.md`.
- Add a pull request check that asks contributors to include a changelog entry.
- `sem entities` accepts multiple file or directory path arguments.

### Changed

- Cloud sync only auto-registers repos that GitHub confirms are public. Private repos run locally unless you opt in with `SEM_SYNC_PRIVATE=1`.
- `install.sh` verifies the release archive against `checksums.txt` before installing.

### Fixed

- Kotlin: resolve method calls through typed receivers that the `tree-sitter-kotlin-ng` grammar exposes positionally (no `name`/`type` fields). Several scope-resolution paths used field names from the older grammar and silently produced no call edges. Fixed:
  - typed function parameters — `fun f(s: Scenario) { s.method() }`;
  - class field types from property declarations (`val conn: Connection`) and primary-constructor properties (`class Tx(val conn: Connection)`);
  - chained field access — `val s = container.scenario; s.method()` resolves `s` via the class field-type map;
  - declared and inferred return types (`fun get(): Connection` / `fun get() = Connection()`), so `val c = get(); c.method()` resolves.
  Kotlin scope-resolution recall on the test fixture rises from 82% to 100%. `sem context`/`impact`/`log` now find these callers.
- Java: resolve cross-file `receiver.method()` call edges. Local variable types were never recorded (the `Dog d = new Dog()` declaration type and `object_creation_expression` RHS were both ignored), class field types were never tracked (`init_strategy` was `None`), and `ClassName.staticMethod()` calls were dropped. As a result `sem impact`/`context` reported few or no cross-file dependents on Java code — an empty result was a false negative. Java scope-resolution recall on the test fixture rises from 27% to 100%; the common Spring field-injection pattern (`@Inject private FooService foo; ... foo.bar()`) now resolves.
- Java: name field entities by their declarator instead of their type. `private FooService fooService;` was extracted as an entity named `FooService` (its type) rather than `fooService`, because `field_declaration` has no `name` field and the generic fallback returned the first type identifier. This collided class and field names in the symbol table. `sem entities`/`diff`/`log` now report the correct field name.
