<h1 align="center">ym</h1>

<p align="center">
  <strong>Modern Java build tool written in Rust</strong><br>
  Drop-in replacement for Maven/Gradle · npm-style DX · Native speed
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License" /></a>
  <a href="https://github.com/ympkg/yummy/releases"><img src="https://img.shields.io/github/v/release/ympkg/yummy?include_prereleases" alt="Release" /></a>
  <a href="https://crates.io/crates/ym"><img src="https://img.shields.io/crates/v/ym" alt="Crates.io" /></a>
  <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20windows-lightgrey" alt="Platforms" />
</p>

<p align="center">
  <a href="#features">Features</a> ·
  <a href="#installation">Installation</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#commands">Commands</a> ·
  <a href="#configuration">Configuration</a>
</p>

<p align="center">
  🌐 <a href="README.md">English</a> · <a href="docs/i18n/zh-CN/README.md">简体中文</a>
</p>

---

## Features

- **Yarn + Vite DX** — Two binaries: `ym` (package manager, like Yarn) + `ymc` (compiler & runtime, like Vite)
- **Native Speed** — ~5ms startup, incremental build < 3s, 2000 modules IDEA project in 8s
- **Zero Config** — Declarative `package.toml`, sensible defaults, no lockfile
- **Monorepo Native** — Cargo-style `{ workspace = true }`, topological builds, parallel compilation
- **Maven Ecosystem** — Direct access to Maven Central, reuses `.jar` / `.pom` format
- **Gradle Migration** — `ym convert` auto-generates `package.toml` from `build.gradle` / `pom.xml`

## Why ym?

| | Gradle | ym |
|---|---|---|
| Startup | ~5s (JVM + Daemon) | ~5ms (native binary) |
| Config format | Groovy/Kotlin DSL | Declarative TOML |
| Dependency resolution | Re-resolves on each build | Cached, zero network on hit |
| Monorepo | Plugin-based | Native workspace support |

## Installation

**One-line install** (Linux / macOS / Git Bash):

```bash
curl -fsSL https://raw.githubusercontent.com/ympkg/yummy/main/install.sh | bash
```

**Windows PowerShell:**

```powershell
irm https://raw.githubusercontent.com/ympkg/yummy/main/install.ps1 | iex
```

**From crates.io** (requires JDK 11+):

```bash
cargo install ym
```

**Manual download**: grab the latest binary from [GitHub Releases](https://github.com/ympkg/yummy/releases) and place it in your `PATH`.

## Quick Start

```bash
ym init my-app            # Create project (zero questions)
cd my-app
ym add com.google.guava:guava   # Add dependency
ymc build                 # Compile (auto-downloads deps)
ymc dev                   # Compile + run + hot reload
ymc test                  # Run JUnit 5 tests
```

## Commands

### Package Manager (`ym`)

```bash
ym init [name]              # Create new project (-i for interactive, -t for template)
ym add <dep>                # Add dependency (auto-fetches latest version)
ym add <dep> --scope test   # Add with scope (compile/runtime/provided/test)
ym remove <dep>             # Remove dependency (fuzzy match supported)
ym upgrade                  # Upgrade dependencies (-i for interactive)
ym convert                  # Convert from Maven/Gradle to package.toml
ym tree                     # Show dependency tree (--depth, --flat, --dot, --reverse)
ym doctor                   # Diagnose environment issues
ym info                     # Show project and environment info
ym publish                  # Publish to Maven registry (--dry-run to preview)
ym login                    # Login to a Maven registry
ym cache clean              # Clean dependency cache (--pattern <p> for selective)
ym workspace list           # List workspace packages
ym workspace foreach -- CMD # Run command in each package (--parallel, --keep-going)
ym <script>                 # Run script from [scripts] in package.toml
```

### Compiler & Runtime (`ymc`)

```bash
ymc build                   # Compile (incremental by default)
ymc build --release         # Build fat JAR for production
ymc build --profile         # Show per-phase timing breakdown
ymc build --keep-going      # Continue on module failure (workspace)
ymc dev                     # Dev mode: compile + run + watch + hot reload
ymc dev --debug             # Dev mode with JDWP debugger (port 5005)
ymc test                    # Run JUnit 5 tests
ymc test --watch            # Watch mode: re-run on change
ymc test --coverage         # Generate JaCoCo coverage report
ymc test --filter "MyTest"  # Filter test classes/methods
ymc test --tag integration  # Run tests by JUnit @Tag
ymc build --clean           # Clean build outputs and rebuild
ymc idea                    # Generate IntelliJ IDEA project files
```

## Configuration

Projects are configured with `package.toml`:

```toml
name = "my-app"
groupId = "com.example"
version = "1.0.0"
target = "21"                    # Java version target

[dependencies]
"com.google.guava:guava" = "33.0.0-jre"
"org.springframework.boot:spring-boot-starter-web" = "3.4.0"
"org.junit.jupiter:junit-jupiter" = { version = "5.11.0", scope = "test" }

[scripts]
hello = "echo Hello from ym!"

[env]
SPRING_PROFILES_ACTIVE = "dev"
```

### Workspace (Monorepo)

```toml
# Root package.toml
name = "my-monorepo"
groupId = "com.example"
workspaces = ["modules/*"]

[dependencies]
"com.google.guava:guava" = "33.0.0-jre"

# Module package.toml — inherits from root
[dependencies]
"com.google.guava:guava" = { workspace = true }
"my-other-module" = { workspace = true }       # Inter-module dependency
```

## Migrating from Gradle/Maven

```bash
cd my-existing-project
ym convert                   # Auto-generates package.toml from build.gradle or pom.xml
```

## License

[MIT](LICENSE)
