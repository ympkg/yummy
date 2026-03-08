# ym — Modern Java Build Tool

Fast, zero-config Java build tool. Drop-in replacement for Maven/Gradle with npm-style DX.

Two binaries, one codebase:
- **`ym`** — package manager & script runner (like Yarn)
- **`ymc`** — compiler & runtime engine (like Vite)

## Install

**Linux / macOS / Git Bash (Windows):**

```bash
curl -fsSL https://raw.githubusercontent.com/ympkg/yummy/main/install.sh | bash
```

**Windows PowerShell:**

```powershell
irm https://raw.githubusercontent.com/ympkg/yummy/main/install.ps1 | iex
```

Installs `ym` and `ymc` to `~/.ym/bin/`.

## Quick Start

```bash
ym init my-app            # Create project (zero questions)
cd my-app
ym add com.google.guava:guava   # Add dependency
ymc build                 # Compile (auto-downloads deps)
ymc dev                   # Compile + run + hot reload
ymc test                  # Run JUnit 5 tests
```

## Why ym?

| | Gradle (2000 modules) | ym (2000 modules) |
|---|---|---|
| Config load | ~20 min (execute 2000 Groovy scripts) | ~2 sec (parse 2000 TOML files) |
| Startup | ~5s (JVM + Daemon) | ~5ms (native binary) |
| Incremental build | ~30s | ~5s |
| Config format | Groovy/Kotlin DSL | Declarative TOML |

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
ymc clean                   # Clean build outputs
ymc clean --all             # Also remove Maven dependency cache
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

MIT
