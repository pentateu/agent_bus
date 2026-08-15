---
name: monorepo-protocol-standards
description: Use when writing, editing, or reviewing the shared WebSocket protocol between TypeScript (packages/protocol) and Rust (crates/protocol). Covers zod schema design, envelope protocol, cross-language parity, bun workspaces, package publishing, monorepo tooling, and error handling at the WS boundary. Load before editing any protocol file in either language.
---

# Monorepo + shared protocol standards for HomeTutor

The full sourced rules are in `docs/monorepo-protocol-best-practices-research.md`. **Read that file before writing or editing protocol code or monorepo config.** Below is the index.

## What the research doc covers (by section)

1. **Bun workspace** — `workspaces` glob array in root `package.json`, `"@scope/name": "workspace:*"` for sibling deps (not pinned semver), hoisting, `bun install`, `--filter` for cross-workspace scripts, `bun.lock` at root, catalogs for shared dep versions.
2. **Shared protocol package (zod + TS)** — export both schema and inferred type (`export type Foo = z.infer<typeof FooSchema>`), barrel `index.ts` with `export *`, framework-agnostic (no React/DOM), `z.discriminatedUnion` for tagged envelopes, version with `v: z.literal(1)`, parse-at-the-boundary (validate once, trust downstream), ESM-only (`"type": "module"`), JSON Schema export as neutral IDL (`z.toJSONSchema`).
3. **WebSocket envelope protocol** — envelope shape `{type, payload, id?, v}`, `discriminatedUnion` on `type` in both directions (ClientMessage, ServerMessage), `id` for request/response correlation (omit for notifications), `v` on every message (not just connect), error messages echo `id` + stable `code`, app-level heartbeats (browser WS has no ping/pong API), backpressure (`readyState === OPEN` + `bufferedAmount`), parse every wire message with zod before branching, reconnect with backoff.
4. **Cross-language parity (TS ↔ Rust)** — canonical-source principle (pick one side, mirror on the other; TS+zod is usually canonical), JSON Schema as neutral IDL (`z.toJSONSchema` for TS, `schemars` for Rust, diff to catch drift), hand-mirror is fine for small protocols if you have a contract test, parity test = round-trip sample messages through both sides, semver on the npm package + `v` on the wire (they move independently), additive-only evolution (new optional field = no `v` bump; new required/removed/semantic change = bump).
5. **TS package publishing** — `package.json` fields (`name`, `version`, `type: "module"`, `exports` with `types` FIRST, `files`, `sideEffects`), `exports` encapsulates the package, ESM-only avoids dual-package hazard, `"types"` points to `.d.ts`, do NOT use `paths` for monorepo packages (use `workspace:*`).
6. **Monorepo tooling** — root `typecheck` script (`tsc --noEmit` per package, driven with `bun run --filter`), per-package `tsconfig.json` extends shared base, `tsc --build` with composite + `references` for incremental, `resolve.dedupe` in Vite (critical for React — single copy across apps), root `lint` + `format` scripts, build everything in CI for a small monorepo.
7. **Testing** — each package has its own suite, unit tests next to source (`*.test.ts`), round-trip tests for the protocol package (`parse → serialize → parse` = identity), mock WebSocket/fetch with `mock` from `bun:test`, snapshot tests are a smell for protocol data, `bun test --bail` in CI.
8. **TS path aliases** — prefer `workspace:*` over `paths` (the package IS the alias), `paths` doesn't affect emit (dangerous for published libs), if you must use `paths` mirror in Vite's `resolve.alias`, `baseUrl` not required since TS 4.1.
9. **Error handling at the boundary** — errors crossing a process boundary must be serializable (no stack traces, no class instances), stable `code: string` per error type (mirror the Rust `ws_error` enum as a `z.enum` in TS), client carries a typed error union, retries only for transient codes, user-visible messages from a known client-side set (not raw server `message`), WS `error` event (transport) vs in-envelope `error` message (application).

## How to use this

When editing protocol code:
- Read the relevant section(s) from `docs/monorepo-protocol-best-practices-research.md` for the construct you're working with.
- If you add/change a message type, update **both** `packages/protocol/src/index.ts` (zod) and `crates/protocol/src/lib.rs` (serde). One is canonical, the other is the mirror — keep them in sync.
- After editing, run `cargo check -p home-tutor-protocol` (or `cargo check --workspace`) and `cd packages/protocol && ./node_modules/.bin/tsc --noEmit` (or the app-level typecheck).
- If you add a new message type, add a round-trip test in both sides.
