# Rust style guide — xyk-arbitrage

Target: match `~/repos/solana-offchain` exactly. Rules are ordered by how often they will bite you while writing new code.

## 1. Comments

- Write every comment as a `/* ... */` block; never write `//`, `///`, or `//!`.
- Never document a public item — `pub fn`/`pub struct`/`pub trait`/`pub enum` stay bare, and long-form prose goes in a `README.md`, not in the source.
- Comment WHY, never WHAT: a comment earns its place only for a non-obvious consequence, a protocol/provider gotcha, or a recovery invariant.
- Keep comments rare — roughly one per 45 lines of code, with most files having none.
- Put the comment inside the function body, immediately above the branch or statement it explains, not in a doc-comment slot above the item.
- Continue a multi-line block flush-left at the same indent with no per-line `*`, and close `*/` at the end of the last text line.

```rust
/* The quote we got from the stream may be a slot behind the pool account,
so re-read before sizing the input */
```

- Mark an error branch that should be unreachable in practice with the exact phrase `/* Normally should not happen */`, then log it and return early.
- Use a bare `/* Subsystem */` block comment as a section divider inside a long enum (this is a `logger.rs` idiom only).
- Duplicate a repeated explanatory sentence verbatim across sibling modules rather than factoring it into a shared doc.

## 2. Errors and panics

- Never write `.unwrap()`, `.expect()`, `panic!`, `unreachable!`, `todo!`, or `unimplemented!` anywhere, tests included.
- Use `anyhow::Result` at every application boundary — in library modules exactly as in the binary — and add no other error crate (no thiserror, eyre, snafu).
- Never define a crate-level `Error` type, a `Result<T>` alias, or an `impl std::error::Error`; import `anyhow::Result` or write `anyhow::Result<T>` inline.
- Attach context with `.context("Short static noun phrase")`; never `with_context`, never a formatted string.
- Promote `Option` to `Result` with `.context(...)`, never `ok_or`/`ok_or_else`.
- Return `Option` from a helper reporting a structurally-absent field, and let the caller decide whether that is an error.
- Reach for `anyhow!`/`bail!` only when there is no source error to wrap.
- Use `map_err` only to convert into a classification enum variant or an `anyhow!` — never to stringify an error into a `String` error type.
- When a caller must branch on recovery strategy, define a tiny local enum wrapping the opaque error with no derives, no `Display`, and no `Error` impl.

```rust
enum PushError {
    Transport(anyhow::Error),
    Broker(anyhow::Error),
}
```

- Keep such enums internal and collapse back to `anyhow::Error` at the public boundary via an `into_inner()` helper.
- Use `unwrap_or`/`unwrap_or_else`/`unwrap_or_default` only to supply a default, never to assert an invariant.
- At a branch where `.unwrap()` would be idiomatic, log the condition and degrade — let-else plus early return.
- Recover from mutex poisoning with `.lock().unwrap_or_else(PoisonError::into_inner)`.
- Propagate with `?` only on the startup path; `main` returns `anyhow::Result<()>` and a construction failure exits the process.

## 3. Naming and item order inside a file

- Suffix types by role: `*App` for a wired-up component, `*Actor` for a spawned mailbox task, `*Config` for deserialized config, `*MetricsApp` with a serializable `*Snapshot` twin, `*Adapter`/`*Filter` for plugin implementors, `*Log` for log payloads.
- Name a module directory's entry type after the directory (`TransactionsApp` in `transactions/mod.rs`) and keep it in that `programs`, never split out into a sibling file.
- Spell identifiers out in full domain words — no `txn`, `ctx`, `mgr`, `msg`, `req`, `res`, `val`, `idx` — and reserve short names for tight closures (`ix`, `key`, `len`, `e`).
- Bind a caught error as `e`, and only use a longer name (`push_error`, `pool_error`) when two errors are live in one scope.
- Singularize the collection name for a loop binding (`for pool in pools`).
- Write constructors as `new(...) -> Self` when infallible, `-> Result<Self>` when fallible, and `-> Arc<Self>` when the constructor spawns the type's own task; name a loop-until-success constructor `connect`.
- Write conversions as free functions named `to_*`, `decode_*`, `*_from_row`, `hex_encode` placed below the impl blocks — never `impl From`/`TryFrom`.
- Keep retry/recreate policy in the public method and delegate the single attempt to a private `_`-prefixed twin (`push` / `_push`).
- Order items in a file: consts → primary type → inherent `impl` → `impl Trait for` → free helper fns → private `*Log` payload structs → `#[cfg(test)] mod tests` last.
- Declare consts `UPPER_SNAKE` with an explicit type right after the imports, using `_` digit separators (`30_000`), and move a const shared across a subtree into that subtree's `constants.rs`.
- Group semantically related struct fields with blank lines, not with comments.
- Omit `let` type annotations except where inference genuinely needs help (trait-object collections, `collect()` targets, deserialize targets).
- Use turbofish on `collect::<...>()` instead of annotating an intermediate binding.
- Keep derive lists minimal and fixed per role: config `#[derive(Clone, Debug, Deserialize)]`, snapshots `#[derive(Serialize)]`, log payloads `#[derive(Valuable)]`, atomic metric holders `#[derive(Default)]`.
- Stay within rustfmt's default 100 columns for code; only comment text and inline string literals may overflow.

## 4. Module layout, visibility, imports

- Give every directory module a `programs`; never use the `foo.rs` + `foo/` sibling form.
- Put all `mod`/`pub mod` declarations at the very top of the file, alphabetically sorted regardless of visibility, then a blank line, then `pub use` re-exports if any, then the `use` block.
- Use only `pub` or fully private visibility — never `pub(crate)`/`pub(super)`; encapsulate by making the *module* private (`mod pools;`) and leaving its items plain `pub`.
- Re-export with `pub use` only to hoist a module's public entry type one level; never glob-re-export your own modules.
- Write imports as one flat rustfmt-sorted block with no blank-line grouping between std / external / local.
- Put braces only on the final path segment and never nest them (`use lapin::options::{A, B};` then `use lapin::{C, D};`, never `use lapin::{options::{A, B}, C}`).
- Import with absolute `use crate::...` for anything outside the current subtree, bare child-module paths for modules declared in that same file, and `super::` only for the immediate parent's own items.
- Never glob-import except for generated code re-exports.
- Import a module path and call through it (`quoter::price(...)`) rather than importing every free function by name; use `use path::{self, Item}` when you need both.
- Name leaf files by the role they play, not the type they contain: `config.rs`, `constants.rs`, `events.rs`, `logger.rs`.
- Keep all logic in modules under `src/app/`; promote code into a separate crate only when it is a genuine cross-binary contract or vendored/generated material.

## 5. Logging

- Route every log through three free functions in one `logger`/`logger-base` module — `info`, `warn`, `error` — and never call a `tracing::` macro, `println!`, `eprintln!`, or `dbg!` at a call site.

```rust
pub fn error(title: impl Display, message: impl valuable::Valuable) {
    tracing::error!(title = %title, message = tracing::field::valuable(&message));
}
```

- Emit exactly two structured fields, `title` and `message`, and nothing else — no ad-hoc key-values, no format string in the event body.
- Pass the title as a variant of a single flat `LoggerTitle` enum in `src/logger.rs` with `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` and a `Display` impl that is just `write!(f, "{self:?}")`.
- Name titles PascalCase as `<Subject><Outcome>` with the subject first, and group variants under `/* Subject */` comment headers.
- Suffix a failure title with `Error` or a one-word rejection state (`Rejected`/`Closed`/`Full`/`Empty`/`Invalid`), and a success title with a past-tense verb (`Connected`/`Queued`/`Matched`/`Starting`).
- Pick one of three payload shapes: `None::<String>` when the title says everything, `Some(e.to_string())` for a single upstream error, or a `#[derive(Valuable)] struct <Title>Log { .. }` once you need two or more fields.
- Wrap a single-value payload in `Some(...)` rather than passing a bare `String`.
- Define each `*Log` struct privately at the bottom of the file that logs it, and duplicate it per file rather than sharing a log-types module.
- Give `*Log` structs flat owned primitives (`String`, `u64`, `Vec<String>`) and build them at the call site with `.clone()`/`.to_string()`.
- Never write a human-readable sentence into a log — no prose, no punctuation, no interpolation; the only free text allowed is an upstream `e.to_string()`.
- Use only three levels: `info` for lifecycle transitions and completed units of work, `warn` for degradation you absorb by design, `error` for a broken durability or delivery guarantee (including one you immediately retry).
- Never log from a shared library module — return `Result` or a typed end-of-stream enum and let the owning caller pick the level and title.
- Log only from the orchestrating loop, actor, or request handler; storage, filter, decode, config, and transport-wrapper modules stay silent and propagate with `?`/`.context(...)`.
- Never use spans, `#[instrument]`, or trace-context propagation — correlate by putting identifying fields into the `*Log` struct.
- Never log the config or any secret, even though `Config` derives `Debug`.
- Initialize the subscriber by calling `setup()` as the first statement of `init()`, before anything else, and keep its configuration centralized and fixed (JSON, `EnvFilter` from `RUST_LOG` with a hardcoded fallback, `ChronoUtc` timer, `with_target(false)`).
- Build with `--cfg tracing_unstable` in `.cargo/config.toml`, since `valuable` fields require it.
- Turn anything too noisy for `info` into an atomic counter instead of a `debug!`.

## 6. Async, control flow, retry

- Structure the binary as a thin `main.rs` (`init()` then `app.run().await`) plus an `app::App` whose `pub async fn run(&mut self)` is one infinite `loop {}` that returns `()` and never propagates an error.
- Drive the main loop from a single `source.next().await`: wrap every external stream in a struct exposing `async fn next(&mut self) -> Result<Item, XEnded>`, where `XEnded` is a hand-written non-error enum naming the end conditions.
- Never let an error escape the loop — log it with a `LoggerTitle` and continue; `?` belongs to `init()` only.
- Implement no graceful shutdown: no cancellation token, no shutdown channel, no SIGTERM handler, no drain — correctness on abrupt death comes from idempotent writes and checkpoints.
- Write reconnection inline in the owning loop as "rebuild the whole connection struct and swap it into `self`": log the end reason, call `X::new(...)` again, assign on `Ok`, `sleep(BACKOFF)` on `Err`.
- Back off with a flat 5-second constant named `BACKOFF`/`RECONNECT_BACKOFF`; never exponential, never jittered, never a retry crate.
- Retry forever in place for any operation whose failure would lose data, and say why in a block comment.

```rust
while let Err(e) = self.publish(&payload).await {
    self.metrics.submission_failed();
    error(LoggerTitle::PublishError, Some(e.to_string()));
    sleep(BACKOFF).await;
}
```

- Classify the error before retrying — split transient/transport from terminal, and loop only on the transient class.
- Bound a retry with a small local `const ATTEMPTS`/`*_ATTEMPTS` counter when the failure is expected to clear on its own, and write the helper per binary rather than sharing one.
- Use `tokio::time::timeout` only to detect a silent remote peer, wrapping the await with a named const, and rely on transport keepalives for everything else.
- Hand work between tasks exclusively through `tokio::sync::mpsc`; never `broadcast`, `watch`, or `oneshot`.
- Build an actor as `fn new(...) -> Arc<Self>` that creates the channel, keeps the `Sender` as a field, `tokio::spawn`s a clone of the `Arc` running `self.run(receiver)`, returns the `Arc`, and exposes a *synchronous* `push_*` so producers never await on the hot path.
- Pick bounded vs unbounded mpsc as an explicit durability tier: bounded `channel(N)` + `try_send` + drop/log/count on `Full` for lossy traffic, `unbounded_channel` for traffic that must not be lost.
- Bound concurrency by not being concurrent: one task per stream, everything inside a handler sequential in a `for` loop with `.await` inside — no `JoinSet`, `Semaphore`, `join_all`, or `buffer_unordered`.
- Share collaborators as `Arc<T>` of data that is immutable after construction, injected through constructors and `.clone()`d at each site.
- Keep mutable shared state to one small `std::sync::Mutex`, and never hold the guard across an `.await`.
- Use `tokio::sync::{RwLock, Mutex}` only for connection/handle rebuild: `RwLock` around the live handle plus a `Mutex<()>` rebuild lock so simultaneous rebuilds collapse into one.
- Track counters as private `AtomicU64`/`AtomicUsize`/`AtomicBool` with `Ordering::Relaxed` in a `#[derive(Default)]` struct, mutated by past-tense verb methods, with a `snapshot()` returning an all-`pub` `#[derive(Serialize)] *Snapshot` declared directly below.
- Encode units in metric field suffixes: `_total`, `_count`, `_ns`, `_ms`, `_max_ns`.
- Compose the metrics root as a `#[derive(Clone)]` struct of `Arc<...>` sub-apps plus a `start: Instant`, and clone it into every component.
- Time an await-heavy unit with `Instant::now()` / `.elapsed()` handed straight to a metrics method at the call site.
- Spawn detached background tasks sparingly, retain no `JoinHandle`, and make the spawned function absorb its own fatal errors by logging and returning.
- Flatten control flow with let-else guard clauses and let-chains instead of nested `if let`/`match`.
- Use a plain `#[tokio::main]` multi-threaded runtime with no tuning and a minimal tokio feature set.
- Make a trait async (`#[async_trait]`) only when an implementor actually awaits; keep pure-CPU traits sync with `Send + Sync`.
- Model pluggable behaviour as a `Send + Sync` dyn trait with `fn label(&self) -> &'static str` returning a module-level `pub const LABEL: &str`, stored as `Vec<Box<dyn Trait>>`.
- Keep the real work in an inherent `async fn process()` and let the trait's `handle()` thinly wrap it to record metrics.
- Signal a cross-component state change by pushing a sentinel variant through the existing work channel (`enum Update { Next(T), Gap }`), not through a side-channel.
- Explain every non-obvious async decision with a `/* ... */` block comment at the decision point.

## 7. Configuration

- Load all configuration from one YAML file via an inherent `Config::load()` reading `CONFIG_PATH`, with a hardcoded `configs/local/<name>.yaml` fallback.

```rust
pub fn load() -> anyhow::Result<Self> {
    let path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "configs/local/bot.yaml".to_string());
    Ok(serde_norway::from_str(&std::fs::read_to_string(&path)?)?)
}
```

- Parse with `serde_norway` only — no config-rs, figment, clap, or dotenvy, and no CLI flags.
- Treat `CONFIG_PATH` as the only environment variable the Rust code reads.
- Give each module its own `config.rs` with one `*Config` struct, all fields `pub`, and compose them into a root `Config` mirroring the module tree.
- Derive `Debug, Deserialize` on the root `Config` and add `Clone` to every nested config struct.
- Make every config field required; use `#[serde(default)]` or `#[serde(default = "default_x")]` with a free `fn default_x()` only where a value is genuinely optional.
- Type config fields as plain primitives (`String`, `u16`, `bool`, `Vec<String>`) and parse them into domain types (`Pubkey`, `Duration`) in the consuming constructor.
- Validate by constructing: no `validate()` pass, no schema file — `main` does `Config::load()?` then `App::new(config).await?` and a bad value exits non-zero before the loop starts.
- Do cross-field validation inline in the consuming constructor with `anyhow!("plain english message")`.
- Keep tuning knobs (timeouts, pool sizes, backoff, queue capacity, prefetch) as Rust consts in `constants.rs`, not as config fields.
- Keep the whole `configs/` tree out of git, secrets inline in plaintext in the YAML, and document the schema as prose in the README.
- Scope `.env` to docker-compose only and commit a `.env.example` with empty values.

## 8. Workspace and Cargo

- Split the workspace into `services/*` (binaries, globbed) and `packages/*` (libraries, listed explicitly), and create `packages/` only once something is genuinely shared.
- Name a shared library crate with a `-base` suffix (`logger-base`), except for a pure schema/contract crate.
- Declare every dependency in every member manifest as `{ workspace = true }` — no version strings, no `features`, no `default-features` outside the root.
- Pin third-party versions in the root `[workspace.dependencies]` to the patch level, with feature sets and `default-features = false` declared there.
- Set `edition = "2024"` and `version = "0.1.0"` in each member, and do not use `[workspace.package]`, `[workspace.lints]`, or `rust-version`.
- Tune only `[profile.release]` (mirrored in `[profile.release.build-override]`): `overflow-checks = true`, `lto = "fat"`, `codegen-units = 1`, `opt-level = 3`, `panic = "abort"`, `strip = "symbols"`, `incremental = false`.
- Use Cargo features only for a PGO instrumentation build — one empty `pgo` feature per binary, consumed via `#[cfg(feature = "pgo")]` to race `ctrl_c()` against the stream so profiles flush.
- Keep `.cargo/config.toml` to the one required rustflags line and build from the repo root so it applies.
- Use implicit `src/main.rs` binaries — no `[[bin]]` sections, no explicit `path` keys.

## 9. Tests and tooling

- Write essentially no tests: reserve a `#[cfg(test)] mod tests` for a pure function whose output must byte-match a foreign implementation, and add nothing for I/O paths.
- Structure that test as `mod tests` with a narrow `use super::<item>;` and a single hardcoded golden-vector `assert_eq!` — no helpers, no fixtures, no setup, no comments.
- Name a test after the assertion it makes, third-person present tense snake_case, with no `test_` prefix (`matches_evm_get_order_id`).
- Restrict `assert!`/`assert_eq!` to `#[cfg(test)]`.
- Add no `[dev-dependencies]`, no `tests/` directory, no benches, no mocks, no fixtures.
- Add no CI workflow, no `rustfmt.toml`, no `clippy.toml`, no `deny.toml`, and no `[lints]` table — just stay rustfmt-default clean.
- Never use an item-level `#[allow(...)]`; the only acceptable lint escape is a crate-level allow demanded by a codegen macro.
- Document build/run/profiling in a `README.md` next to the binary's `Cargo.toml`, and ship a multi-stage Dockerfile that builds from the workspace root with `cargo build --release -p <bin>` and runs the single binary as a non-root user on debian-slim.

## Does NOT do

- `//` line comments, `///` or `//!` doc comments, or any prose documentation of a public item.
- `.unwrap()`, `.expect()`, `panic!`, `unreachable!`, `todo!`, `unimplemented!` — anywhere, including startup and tests.
- `thiserror`, `eyre`, `snafu`, `Box<dyn Error>`, a crate `Error` enum, a `Result<T>` alias, or `impl std::error::Error`.
- `with_context(|| format!(..))`, `ok_or`, `ok_or_else`.
- `impl From`/`TryFrom`/`Into` conversions, `*Builder` types, or `with_*` chains on own types.
- `pub(crate)`, `pub(super)`, `pub(in path)`.
- Glob imports, nested-brace imports, and blank-line-separated std/external/local import groups.
- The `foo.rs` + `foo/` module pairing.
- `tracing::` macros at a call site, `println!`, `eprintln!`, `dbg!`, the `log`/`env_logger` stack, format strings in log events, `debug!`/`trace!` levels, spans, `#[instrument]`.
- A shared log-payload module — `*Log` structs are duplicated per file on purpose.
- Graceful shutdown machinery: `CancellationToken`, shutdown channels, SIGTERM handling, drain-on-exit.
- `broadcast`, `watch`, `oneshot`; task supervision, `JoinSet`, `Semaphore`, `join_all`, `buffer_unordered`, `spawn_blocking`.
- Exponential backoff, jitter, max-elapsed-time, or any retry/rate-limit crate (`backoff`, `tokio-retry`, `governor`).
- `parking_lot`, `std::sync::RwLock`, `Arc<Mutex<HashMap>>` state blobs, or any lock held across an `.await`.
- `tokio::time::interval` and periodic tickers — everything is event-driven off a `next()`.
- Feature flags as a configuration mechanism (only the empty `pgo` feature exists); env-var overrides of config values; layered/merged config; `#[serde(deny_unknown_fields)]`; domain types inside config structs; secret-wrapper types (`secrecy`, `Zeroize`).
- Extracting duplicated scaffolding (metrics/config/logger trees, `Config::load`, `App::new`+`run`) into a shared framework — each binary reimplements it.
- Abbreviated identifiers, `pub` fields on internal state, item-level `#[allow]`.
- Tests around I/O code, mocks, fixtures, `tests/`, `[dev-dependencies]`, CI, linter/formatter config, and long-form planning markdown (the author actively deletes all of these).

## Won't transfer (solana-offchain domain only)

These are shaped by RabbitMQ, Postgres, and the Geyser/Yellowstone feed. Keep the *shape* where you have an analogue; drop the rule entirely where you do not.

**Postgres / sqlx** — irrelevant with no database:
- No migrations and no `sqlx::migrate!`; each `*Storage` owns `init_schema()` full of `CREATE TABLE IF NOT EXISTS`, called from its constructor, failure logged and swallowed.
- All SQL inline as `&'static str` in a file literally named `postgres.rs`, colocated with the feature that owns the data; positional `$N` + `.bind()`, never `format!` or `QueryBuilder`.
- Runtime `sqlx::query`/`query_scalar` only — the `macros` feature is deliberately off, so no `query!`, no `.sqlx` cache, no build-time `DATABASE_URL`.
- Hand-written `fn x_from_row(row: &PgRow) -> Result<T, sqlx::Error>`; no `FromRow`, no `query_as`, no ORM.
- `u64` stored as BIGINT with `as i64` / `as u64` casts at the boundary; u256 as TEXT, pubkeys as TEXT, raw EVM bytes as BYTEA, nested payloads as JSONB.
- Storage functions return `Result<T, sqlx::Error>` (not anyhow) so the caller can classify; retryability lives in one `retry.rs` matching `sqlx::Error` variants plus SQLSTATE class prefixes `08|40|53|55|57`, with `42P01` triggering `init_schema()` + one retry.
- `PgPoolOptions::new().max_connections(5).connect_lazy(..)` in `App::new`, pool cloned into each storage type — meaning a bad DB URL does not fail startup.
- `ON CONFLICT DO NOTHING` / `DO UPDATE SET .. = EXCLUDED..` on every insert, explicit `pool.begin()`/`tx.commit()` for multi-row writes, `= ANY($1)` / `UNNEST($2::TEXT[])` for list params.
- *Transferable kernel*: the `verb_noun()` public / `_verb_noun()` private split, "best-effort side writes never abort the main path, bump a `*_failed()` counter and log", and "make writes idempotent and checkpoint progress instead of transactional rollback".

**RabbitMQ** — irrelevant with no broker:
- No topology declaration from code (`exchange_declare`/`queue_declare`/`queue_bind` appear nowhere); exchanges, queues, and bindings come from a per-env `rabbitmq-definitions.json` mounted into the broker.
- A `packages/solana-events` crate holding the exchange-name const plus the serde wire structs as the shared pipe contract, with `serde_json::to_vec`/`from_slice` on both sides.
- Routing keys built as `format!("transactions.{}.{}", filter_label, account)`.
- Publisher confirms always on, `mandatory: true`, 10s confirm timeout, message timestamp set, with only `persistent: bool` varying the delivery mode.
- Transport-vs-broker publish failure split: transport rebuilds the connection and retries once, broker failure goes back to the caller because a retry could duplicate.
- Ack every delivery manually after the handler runs, success or failure, and park the failure in a `failed_txs` table — no nack, reject, requeue, or DLQ.
- The persistent/relaxed actor pair (unbounded + retry-forever + checkpoint vs bounded + `try_send` + drop) is a *delivery-durability* tier split; for an arb bot the same two-tier idea maps onto "must-land order submission" vs "droppable telemetry", but the naming and checkpoint table do not.
- *Transferable kernel*: keep the transport wrapper policy-free (return `anyhow::Result` and a `ConsumeEnded`-style enum) and let the owning `*App` own the reconnect loop, backoff, metrics, and logging.

**Geyser / Yellowstone** — adapt, since the bot streams pool accounts:
- The reconnect-and-swap loop, `StreamEnded { Idle, Closed, Failed }`, `timeout(STREAM_IDLE_TIMEOUT, stream.next())`, and HTTP/2 keepalive consts all transfer directly to a gRPC pool feed.
- The `TxFilter` trait returning `None` on an empty account list (so Geyser does not stream all of mainnet) and the `Update::Gap` sentinel + RPC gap-recovery state machine are listener-specific; only port them if you actually need gap-free history, which an arb bot usually does not.
- The `packages/programs` anchor wrapper (per-program `programs`/`idl.rs`/`events.rs`, `declare_program!` + glob re-export, `match_event!` macro, crate-level `#![allow(unexpected_cfgs)]`, `Option`-returning `decode_event` with no error dependency) transfers verbatim if you decode Anchor pool events, and is dead weight otherwise.

**HTTP API** — only if the bot exposes one:
- Handlers return `Result<Json<T>, StatusCode>` with a bare status code as the error type, the real error logged server-side and nothing leaked in the body; no `impl IntoResponse` for an error type.
- Status by cause: missing/unknown token → 401, wrong role → 403, storage error → 500, degraded dependency on `/health` → 503.
- Request/response DTOs declared immediately above the handler that uses them; row structs double as response bodies via `#[derive(Serialize)]` with private fields.
- Role/permission logic as inherent methods on the config struct; note the existing code compares tokens with plain `==` (not constant-time) — do not copy that if the bot's endpoint is reachable.
- *Transferable kernel*: expose rates, latencies, and liveness through a `/health` metrics snapshot rather than the log stream.

**Deployment** — copy only if the bot is containerized: per-env `docker-compose.{local,prod}.yml` differing solely in which `configs/<env>/` it mounts, selected by `COMPOSE_FILE` in `.env`, with the YAML mounted read-only at `/etc/<service>/config.yaml` and `CONFIG_PATH` pointed at it.