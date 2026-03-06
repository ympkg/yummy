# YM 功能规格文档

## 模块总览

| # | 模块 | Spec 文件 | 当前状态 | 核心文件 |
|---|------|----------|---------|---------|
| 1 | [配置与 Schema](01-config.md) | `01-config.md` | ✅ 已实现 | `config/schema.rs`, `config/mod.rs` |
| 2 | [依赖解析](02-resolver.md) | `02-resolver.md` | ⚠️ 需优化 | `workspace/resolver.rs` |
| 3 | [包管理命令](03-package-management.md) | `03-package-management.md` | ✅ 已实现 | `commands/{add,remove,install,upgrade,outdated,lock,dedupe,pin}.rs` |
| 4 | [编译管线](04-compiler.md) | `04-compiler.md` | ⚠️ 需优化 | `compiler/{mod,incremental,javac,ecj_service}.rs`, `commands/build.rs` |
| 5 | [运行与开发模式](05-runtime.md) | `05-runtime.md` | ⚠️ 需优化 | `commands/{run,dev}.rs`, `hotreload/`, `watcher/` |
| 6 | [测试](06-testing.md) | `06-testing.md` | ✅ 已实现 | `commands/test_cmd.rs` |
| 7 | [工作空间](07-workspace.md) | `07-workspace.md` | ⚠️ 需优化 | `workspace/{graph,cache}.rs`, `commands/{create,workspace_cmd}.rs` |
| 8 | [JDK 管理](08-jdk.md) | `08-jdk.md` | ✅ 已实现 | `jdk_manager.rs`, `jvm.rs` |
| 9 | [IDE 集成](09-ide.md) | `09-ide.md` | ⚠️ 需改进 | `commands/idea.rs` |
| 10 | [发布与分发](10-publish.md) | `10-publish.md` | ✅ 基本完成 | `commands/{publish,login,link}.rs` |
| 11 | [诊断与工具](11-diagnostics.md) | `11-diagnostics.md` | ✅ 已实现 | `commands/{doctor,env,validate,verify,audit,...}.rs` |
| 12 | [迁移](12-migration.md) | `12-migration.md` | ✅ 已实现 | `commands/migrate.rs` |
| 13 | [脚本与生命周期](13-scripts.md) | `13-scripts.md` | ✅ 已实现 | `scripts.rs`, `commands/script.rs` |
| 14 | [CLI 架构](14-cli.md) | `14-cli.md` | ✅ 已实现 | `main.rs` |

## 状态说明

- ✅ **已实现** — 功能完整可用
- ⚠️ **需优化** — 功能可用但有已知性能/功能瓶颈
- 🔲 **待开发** — 尚未实现

## 架构原则

1. **双二进制架构**：`ym`（包管理器）+ `ymc`（编译器/运行时），同一二进制按名称分发
2. **声明式配置**：`package.json`（JSON），无脚本执行，零图灵完备性
3. **Maven 生态兼容**：直接使用 Maven Central，复用 `.jar`/`.pom` 格式
4. **渐进式采用**：支持从 Maven/Gradle 迁移，`source_dir()` 自动检测目录结构
5. **原生性能**：Rust 编写，~5ms 启动，rayon 并行编译/下载
