# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Yummy (`ym`) is a modern Java build tool written in Rust. It provides a package manager (`ym`) and compiler/runtime (`ymc`) as a single binary that dispatches based on executable name.

## Spec Location

Design docs live in the separate spec repo under `spectalk/spec-yummy/02-design/`.

## Development Rules

- When implementing against a spec, first list a checklist of every spec item, implement and tick them off one by one, then verify each. Do not write code from memory.
- **Code language: English only.** This is an international open-source project — all comments, string literals, assert messages, and commit messages must be in English. (The design-spec repo `spec-yummy` stays Chinese by its own convention; that split is intentional.)

## Build Commands

```bash
cargo build                   # Debug build (requires JDK for build.rs to compile ym-agent)
cargo build --release         # Release build (opt-level=3, LTO, stripped)
cargo test                    # Run Rust unit tests
cargo clippy                  # Lint
cargo run -- <ym-args>        # Run as ym
cargo run --bin ymc -- <args> # Run as ymc
```

**Build prerequisite:** JDK must be installed — `build.rs` compiles the embedded `ym-agent/` Java sources into a JAR via `javac` and `jar`.

## Architecture

**Dual binary:** `main.rs` checks `argv[0]` — if `ymc`, calls `run_ymc()`; otherwise calls `run_ym()`. Both share `lib.rs` which defines the two CLI trees via Clap derive.

**Key modules:**

| Directory | Purpose |
|-----------|---------|
| `commands/` | CLI subcommands (add, build, dev, test, migrate, etc.) |
| `compiler/` | Java compilation: worker pool (`worker.rs`), incremental builds (`incremental.rs`) |
| `config/` | `schema.rs` — config model (`YmConfig`, `DependencySpec`, scopes, resolved cache) |
| `workspace/` | `resolver.rs` — Maven dependency resolution, POM crawling; `graph.rs` — petgraph DAG for build order |
| `hotreload/` | Hot reload agent client (TCP to embedded ym-agent via JDWP) |
| `watcher/` | File system watching (notify crate) for `ymc dev` |

**Compiler worker pattern:** Long-running JVM processes (`YmCompilerWorker.java`) communicate via stdin/stdout JSON protocol, eliminating per-module JVM startup.

**Dependency resolution:** BFS traversal of Maven POM tree with scope propagation, cached in `.ym/resolved.json` with config fingerprinting for invalidation.

**Workspace model:** Petgraph-based DAG with topological sort for correct multi-module build order. Parallel compilation via Rayon.
