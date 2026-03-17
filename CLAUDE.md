# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Yummy (`ym`) is a modern Java build tool written in Rust. It provides a package manager (`ym`) and compiler/runtime (`ymc`) as a single binary that dispatches based on executable name.

## Spec 位置

设计文档在 `/mnt/d/code/spectalk/spec-yummy/02-design/` 目录下。

## 开发规则

对照 spec 实现时，必须先列出所有 spec 条目的 checklist，逐条实现并标记完成，最后逐条验证。不要凭印象写代码。

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
