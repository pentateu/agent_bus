---
name: react-ts-vite-standards
description: Use when writing, editing, or reviewing TypeScript or React code in apps/lesson-web, apps/parent-web, or apps/ops-web. Covers TS strict tsconfig, React 19 hooks/actions, zod, Vite, WebSocket-in-React, unified/markdown rendering, RTL/Vitest testing, ESLint/Prettier, and a11y. Load before editing any .ts/.tsx file in the web apps.
---

# React + TS + Vite standards for HomeTutor

The full sourced rules are in `docs/react-ts-vite-best-practices.md`. **Read that file before writing or editing web app code.** Below is the index.

## What the research doc covers (by section)

1. **TypeScript tsconfig** — strict mode flags (`strict`, `noUncheckedIndexedAccess`, `noImplicitOverride`, `exactOptionalPropertyTypes`), `moduleResolution: "bundler"` for Vite, `verbatimModuleSyntax` + `import type`, `isolatedModules`, `satisfies`, discriminated unions, `as const`, avoid `any` (use `unknown` + narrowing or zod parse).
2. **React 19** — Rules of Hooks, refs-as-regular-props (no `forwardRef` needed), `use()` for promises/context, Actions/form actions, `useActionState`/`useFormStatus`, `useMemo`/`useCallback` only for referential equality, effects are for syncing with external systems (not for deriving state), cleanup returns, Strict Mode double-invoke, stable+unique keys (NOT array index), controlled vs uncontrolled inputs.
3. **Effective TypeScript** — `readonly`/`ReadonlyArray`, narrow with discriminated unions, exhaustiveness via `never`, prefer string-literal unions over `enum`, `unknown` over `any`, `satisfies`.
4. **Zod** — parse-at-the-boundary (validate once, trust the type), `z.infer<typeof Schema>`, `safeParse` for recoverable errors, `discriminatedUnion`, schemas as single source of truth, sharing across the monorepo protocol package.
5. **Vite** — `defineConfig`, `resolve.alias` for monorepo, `optimizeDeps`, dynamic `import()` for code splitting, `manualChunks` for vendor splitting, env vars (`import.meta.env.VITE_*`), `vite-plugin-checker` for typecheck-on-build, `tsc --noEmit` alongside Vite (Vite strips types without checking).
6. **WebSocket in React** — don't put socket in `useState` (not serializable), use a ref or module-level singleton, `useSyncExternalStore` for live data, reconnect with backoff, cleanup on unmount, stable callbacks to avoid re-subscribing.
7. **Markdown/math rendering** — the unified pipeline (`remarkParse` → `remarkMath` → `remarkRehype` → `rehypeKatex` → `rehypeSanitize` → `rehypeStringify`), `rehype-sanitize` must come AFTER plugins that add HTML, `useMemo` to avoid re-parsing.
8. **Testing** — RTL guiding principle ("tests resemble how software is used"), `getByRole` over `getByTestId`, `userEvent` over `fireEvent`, `findBy*` for async, avoid implementation details, MSW for HTTP mocking, bun test for the apps that use it.
9. **ESLint/Prettier/Biome** — `@typescript-eslint` recommended + strict, `eslint-plugin-react-hooks` (rules-of-hooks, exhaustive-deps), `noUnusedVariables`, `noExplicitAny`, `consistentTypeImports`, `no-floating-promises` (needs type-aware rules).
10. **Accessibility** — native semantic elements (`<button>` not `<div onClick>`), `aria-label` only when no visible text, focus management for modals (`role="dialog"`, `aria-modal`, trap focus, restore on close), keyboard navigation, `alt` on images.

## How to use this

When editing web app code:
- Read the relevant section(s) from `docs/react-ts-vite-best-practices.md` for the construct you're working with.
- Follow the existing codebase conventions first; the standards doc second.
- After editing, run `cd apps/<app> && ./node_modules/.bin/tsc --noEmit` and `bun test` if the app has tests.
