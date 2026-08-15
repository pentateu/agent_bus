---
name: ui-skills
description: Route UI/UX work through the ui-skills CLI to load the smallest relevant skill. Use for web (React/Vite), SwiftUI iOS, dashboards, polish, audits, motion, accessibility, and design improvements when Impeccable commands are not explicitly requested.
license: MIT
metadata:
  author: ibelick
  version: "1.0.0"
---

# UI Skills Router

You are the routing layer for UI Skills in this repo.

Use this skill when the user wants UI/UX help and has not invoked an Impeccable command (`/impeccable ...`). Impeccable (`.cursor/skills/impeccable/`) is the primary system for design work, polish, audits, and on-brand iteration. UI Skills complements it with modular, task-specific guidance fetched on demand.

## Project context

- **Web apps:** React 19 + Vite — `apps/lesson-web`, `apps/ops-web`
- **iOS app:** SwiftUI — `apps/student-ios`
- **Design docs:** Impeccable uses `PRODUCT.md` and `DESIGN.md` at repo root (create via `/impeccable init` or `/impeccable document`)

## Protocol

1. Decide if the task is UI-related
2. If not, return `no skill needed`
3. Identify the likely category
4. Inspect that category with the CLI
5. Select the smallest useful skill set (prefer 1; max 3)
6. Load only selected skill(s) via `npx ui-skills get <slug>`
7. Implement using that context

## CLI

```bash
npx ui-skills start
npx ui-skills categories
npx ui-skills list --category <category>
npx ui-skills get <slug>
```

## Quick routing for this repo

| Task | Start with |
|------|------------|
| Full design system, polish, audit, critique | Use Impeccable (`/impeccable polish`, `/impeccable audit`, etc.) |
| Dashboard / admin / ops UI | `npx ui-skills get dammyjay93/interface-design` or `jakubkrehel/better-interface` |
| Micro-interactions, shadows, motion feel | `npx ui-skills get jakubkrehel/better-ui` or `mengto/beautiful-shadows` |
| Read-only UI audit / fix plan | `npx ui-skills get ibelick/improve-ui` |
| a11y + UX checklist | `npx ui-skills get vercel-labs/web-design-guidelines` |
| SwiftUI screens | `npx ui-skills get mengto/swiftui-pro` or `dimillian/swiftui-ui-patterns` |
| Palettes, typography, patterns | `npx ui-skills get nextlevelbuilder/ui-ux-pro-max` or Impeccable `/impeccable typeset` |
| Anti-generic baseline | `npx ui-skills get anthropics/frontend-design` |
| React performance / patterns | `npx ui-skills get vercel-labs/react-best-practices` |

## Selection rules

Prefer 1 skill. Use 2 only when the task needs two clear angles. Use 3 only for broad review, redesign, or multi-surface work. Never use more than 3.

Route by topic, then stack, then specificity. Prefer specific skills over broad skills. Prefer framework-specific skills when the stack is obvious.
