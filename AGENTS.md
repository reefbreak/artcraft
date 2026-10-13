# AGENTS.md

This file provides guidance for coding agents when working with the Artcraft monorepo.

## Project Overview

Gesso is a pure-Rust (egui) desktop app for generating AI images, video and audio on the user's
own Google Cloud and AWS accounts. It is a modified version of ArtCraft
(https://github.com/storytold/artcraft); ArtCraft's Tauri app and web frontend have been removed.
See `NOTICE` before adding any ArtCraft-branded asset: Gesso must not use ArtCraft's name or logos
as its own.

## Project Structure

```
gesso/
├── _database/sql/artcraft_migrations/  # Embedded SQLite task database migrations
├── .sqlx/                             # SQLite query metadata for offline builds
├── gesso/                             # The Gesso desktop app (egui), its assets and tests
├── crates/
│   ├── api_clients/                  # Provider clients and shared API types
│   ├── lib/                          # Support libraries
│   ├── schema/database/sqlite_tasks/ # Task persistence
│   ├── schema/public/                # Shared wire-format enums and identifiers
│   └── testing/                      # Shared test helpers
├── script/rust/                      # Test, lint and cleanup scripts
├── test_data/                        # Fixtures used by retained crate tests
└── Cargo.toml                        # Workspace; default member is gesso
```

Keep `external/` and `secrets/` untouched. Repository pruning must preserve
untracked and ignored local files; delete only Git-tracked paths unless the user
explicitly authorizes a cache cleanup.

## Code Style

- Rust with no minimum supported version
- SQLx for SQLite; prefer `sqlx::query!` / `sqlx::query_as!` compile-time checked macros over runtime `sqlx::query()` whenever possible
- A mix of wreq and reqwest for Rust HTTP clients
- Never use `println!` or `eprintln!` outside of tests; use `log` crate macros instead
- When two crates export the same type name, alias imports with a suffix: `use foo::Bar as BarFoo;`
- Prefer `use` imports over inline fully-qualified paths; only qualify inline for true one-offs or std collisions
- Use two spaces for indentation

### File Layout

Organize for top-to-bottom reading. Important things first, details later.

- **Constants** at the top (after imports)
- **Structs/enums** next; outer structs above inner sub-structs
- **API types** in order: Request, Response, Error
- **In impl blocks**: constructors first, then public methods, then private helpers
- Private helpers go *below* the methods that call them
- Among helpers: meatier logic above leaf-level formatters
- **In test modules**: constants first, then test cases (grouped into sub-modules when 2+), then helper functions last

## Markdown

- **Tables must be space-padded so columns align in plain text.** Markdown
  tables are read raw (terminals, diffs, editors) at least as often as
  rendered, and condensed tables are unreadable there. Pad every cell to its
  column width:

  ```markdown
  | Model        | Configuration | Credits    | Speed | Score   |
  |--------------|---------------|--------------------|---------|
  | Meshy 6      | text or image | 104        | 80.0  | +24     |
  | Rodin 2.5    | text or image | 13         | 10.0  | +3      |
  ```

  Not: `| Model | Configuration | Credits | Speed | Score |` packed tight
  with varying widths per row.
