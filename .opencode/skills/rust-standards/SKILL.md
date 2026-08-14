---
name: rust-standards
description: Use when writing, editing, or reviewing Rust code in crates/** (domain, db, api, protocol, orchestrator, llm). Covers API design, error handling, async/sqlx/serde idioms, performance, testing, clippy lints, and cargo workspace conventions. Load before editing any .rs file.
---

# Rust standards for HomeTutor

The full sourced rules are in `docs/rust-best-practices-research.md`. **Read that file before writing or editing Rust in this repo.** Below is the index of what's covered so you know what to look for.

## What the research doc covers (by section)

1. **Rust API Guidelines** — naming (snake_case fns/vars, UpperCamelCase types, SCREAMING_SNAKE consts), conversion prefixes (`as_`/`to_`/`into_`), getter/iterator conventions, interop (serde, `From`/`TryFrom` only, `Error: Send+Sync+'static`), `#[non_exhaustive]`, debuggability.
2. **Effective Rust** — 35 items: errors (thiserror for libs, anyhow for apps), lifetimes, generics vs trait objects, "don't panic", testing, deps/tooling.
3. **Rust Design Patterns** — Builder, Newtype, borrowed-type args, `mem::take`/`replace`, finalisation-in-Drop, `#[non_exhaustive]`.
4. **The async book** — futures are lazy, drop=cancellation, cancellation safety, blocking/spawn_blocking, `select!` bias, `Send + 'static` spawn bounds, `async fn` in traits.
5. **Clippy restriction lints** — ~25 lints worth enabling (`unwrap_used`, `expect_used`, `panic`, `dbg_macro`, `print_stdout`, `exit`, `mem_forget`, `as_conversions`, `indexing_slicing`, etc.).
6. **Rust Performance Book** — allocation reuse, `&[T]` not `&Vec<T>`, `bytes::Bytes` for network data, avoid `String` in hot paths, `ahash`/`fxhash`, `Cow<str>` at boundaries.
7. **Tokio async idioms** — `spawn_blocking`, no locks across `.await`, `CancellationToken`, `tokio::sync::RwLock` vs `std::sync::RwLock`, buffer sizing.
8. **sqlx best practices** — `query_as` with `FromRow`, prepared statements, `query!` macro vs runtime, transaction patterns, N+1 avoidance.
9. **Serde best practices** — `#[serde(rename_all = "camelCase")]` at API boundaries, `#[serde(default)]`, `Option<T>`, `#[serde(tag = "type")]`, `Cow<'a, str>` for zero-copy.
10. **Unsafe code** — when `unsafe` is justified, the four superpowers, `Send`/`Sync` correctness, safe wrappers.
11. **Cargo workspace** — `[workspace.dependencies]`, feature unification, `pub use` re-exports, semver, `[features]` design (additive, not exclusive).
12. **tracing/logging** — spans vs events, `#[tracing::instrument]`, structured fields (`student_id = %id` not `format!`), level discipline.
13. **rustfmt/clippy** — use rustfmt, pedantic vs restriction lints, the unstable rustfmt options.

## How to use this

When editing Rust:
- Read the relevant section(s) from `docs/rust-best-practices-research.md` for the construct you're working with (e.g. §9 for serde, §8 for sqlx, §4 for async).
- Follow the existing codebase conventions first; the standards doc second. If they conflict, follow the code and flag the drift.
- After editing, run `cargo check --workspace`, `cargo clippy --workspace --all-targets`, `cargo test --workspace`.
