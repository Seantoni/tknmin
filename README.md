# Tokens

Mac desktop app for tracking local AI token usage (Cursor, Claude Code, Codex).

Stack: Tauri 2 + React + TypeScript + Rust.

See [PLAN.md](./PLAN.md) for the full MVP plan.

## Prerequisites

- Node.js + pnpm
- Rust (via rustup)
- Xcode / macOS developer tools

## Commands

```sh
pnpm install
pnpm tauri dev      # development window
pnpm tauri build    # standalone macOS .app

cd src-tauri && cargo test   # Rust unit tests
```

## Architecture

Each Rust layer depends only on the ones below it:

| Layer | Location | Responsibility |
| --- | --- | --- |
| Commands | [src-tauri/src/commands.rs](src-tauri/src/commands.rs) | The only surface React can call |
| State | [src-tauri/src/state.rs](src-tauri/src/state.rs) | Holds the repository behind its trait |
| Pipeline | [src-tauri/src/pipeline.rs](src-tauri/src/pipeline.rs) | Drafts in, stored records out |
| Repository | [src-tauri/src/repository/](src-tauri/src/repository/) | Storage and aggregation; in-memory for now |
| Normalizer | [src-tauri/src/normalize/](src-tauri/src/normalize/) | The only producer of validated records |
| Adapters | [src-tauri/src/adapters/](src-tauri/src/adapters/) | One per source application, producing drafts |
| Fixtures | [src-tauri/src/fixtures.rs](src-tauri/src/fixtures.rs) | Deterministic sample data, standing in for the adapters |
| Domain | [src-tauri/src/domain/](src-tauri/src/domain/) | Shared vocabulary, depending on nothing |

```text
discovery → source adapter → UsageRecordDraft → normalizer/validator
          → repository → Tauri command → React
```

On the React side, [src/domain/usage.ts](src/domain/usage.ts) mirrors the Rust types and [src/api/usage.ts](src/api/usage.ts) is the only module that calls `invoke`.

## Data status

No logs are read yet. The dashboard shows the deterministic sample dataset in [src-tauri/src/fixtures.rs](src-tauri/src/fixtures.rs), which is identical on every launch. Real Cursor, Claude Code, and Codex imports arrive in Phase 6.

After a successful release build, a copy of the app used for early packaging verification lives at:

```text
dist-app/Tokens.app
```

Tauri also writes bundles under its Cargo target directory:

```text
src-tauri/target/release/bundle/macos/Tokens.app
src-tauri/target/release/bundle/dmg/Tokens_0.1.0_aarch64.dmg
```

The build is unsigned and unnotarized, which is fine for personal use on this Mac. Both are needed before the app installs smoothly on another Mac.
