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
| Refresh | [src-tauri/src/refresh.rs](src-tauri/src/refresh.rs) | The single owner of when anything is read or written |
| State | [src-tauri/src/state.rs](src-tauri/src/state.rs) | Holds a read-only repository handle and the coordinator handle |
| Pipeline | [src-tauri/src/pipeline.rs](src-tauri/src/pipeline.rs) | Drafts in, validated records out |
| Repository | [src-tauri/src/repository/](src-tauri/src/repository/) | Durable SQLite transactions, upserts, checkpoints, revisions |
| Normalizer | [src-tauri/src/normalize/](src-tauri/src/normalize/) | The only producer of validated records |
| Adapters | [src-tauri/src/adapters/](src-tauri/src/adapters/) | One per source application, producing the requested delta |
| Fixtures | [src-tauri/src/fixtures.rs](src-tauri/src/fixtures.rs) | Deterministic sample data, used by tests |
| Domain | [src-tauri/src/domain/](src-tauri/src/domain/) | Shared vocabulary, depending on nothing |

```text
trigger → refresh coordinator → source adapter (delta only)
        → normalizer/validator → one repository transaction
        → committed revision → Tauri event → React
```

On the React side, [src/domain/usage.ts](src/domain/usage.ts) mirrors the Rust types and [src/api/usage.ts](src/api/usage.ts) is the only module that calls `invoke`.

## How data stays current

Automatically. The app watches each source's files, reads only the bytes that
changed, commits them in one transaction, and publishes the resulting revision;
React subscribes to that revision and refetches one atomic snapshot. Filesystem
watchers provide the speed and periodic reconciliation provides the
correctness, so a dropped notification repairs itself rather than requiring a
person to notice.

**Manual "sync now" is recovery and diagnostics only.** It submits the same
trigger every automatic path submits. Removing the button would not change
whether the app stays correct.

One rule holds the design together: [src-tauri/src/refresh.rs](src-tauri/src/refresh.rs)
is the *only* production owner of refresh and scan orchestration. No other file
starts a timer, a watcher, a retry, or a scan, and the repository's write
capability is handed to the coordinator alone — so a new command cannot become
a second refresh path even by accident.

## Data status

Usage records, quotas, per-source checkpoints, and sync health persist to
app-owned SQLite in WAL mode, so a relaunch shows the last committed data
immediately and catches up behind it. The deterministic sample dataset in
[src-tauri/src/fixtures.rs](src-tauri/src/fixtures.rs) is now used only by
tests.

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
