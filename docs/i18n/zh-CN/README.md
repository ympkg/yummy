<h1 align="center">ym</h1>

<p align="center">
  <strong>用 Rust 编写的现代 Java 构建工具</strong><br>
  Maven/Gradle 的替代品 · npm 风格开发体验 · 原生速度
</p>

<p align="center">
  <a href="../../../LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="License" /></a>
  <a href="https://github.com/ympkg/yummy/releases"><img src="https://img.shields.io/github/v/release/ympkg/yummy?include_prereleases" alt="Release" /></a>
  <a href="https://crates.io/crates/ym"><img src="https://img.shields.io/crates/v/ym" alt="Crates.io" /></a>
  <img src="https://img.shields.io/badge/platforms-linux%20%7C%20macOS%20%7C%20windows-lightgrey" alt="Platforms" />
</p>

<p align="center">
  <a href="#特性">特性</a> ·
  <a href="#安装">安装</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#命令">命令</a> ·
  <a href="#配置">配置</a>
</p>

<p align="center">
  🌐 <a href="../../../README.md">English</a> · <a href="README.md">简体中文</a>
</p>

---

## 特性

- **Yarn + Vite 体验** — 双命令架构：`ym`（包管理，对标 Yarn）+ `ymc`（编译运行，对标 Vite）
- **原生速度** — ~5ms 启动，增量编译 < 3s，2000 模块 IDEA 工程生成 8s
- **零配置** — 声明式 `package.toml`，合理默认值，无 lockfile
- **Monorepo 原生** — Cargo 风格 `{ workspace = true }`，拓扑排序编译，并行构建
- **Maven 生态兼容** — 直接使用 Maven Central，复用 `.jar` / `.pom` 格式
- **Gradle 迁移** — `ym convert` 从 `build.gradle` / `pom.xml` 自动生成 `package.toml`

## 为什么选 ym？

| | Gradle | ym |
|---|---|---|
| 启动 | ~5 秒（JVM + Daemon） | ~5 毫秒（原生二进制） |
| 配置格式 | Groovy/Kotlin DSL | 声明式 TOML |
| 依赖解析 | 每次构建重新解析 | 缓存命中时零网络请求 |
| Monorepo | 基于插件 | 原生工作空间支持 |

## 安装

**一键安装**（Linux / macOS / Git Bash）：

```bash
curl -fsSL https://raw.githubusercontent.com/ympkg/yummy/main/install.sh | bash
```

**Windows PowerShell：**

```powershell
irm https://raw.githubusercontent.com/ympkg/yummy/main/install.ps1 | iex
```

**通过 crates.io**（需要 JDK 11+）：

```bash
cargo install ym
```

**手动下载**：从 [GitHub Releases](https://github.com/ympkg/yummy/releases) 下载对应平台的二进制文件，放入 `PATH` 即可。

## 快速开始

```bash
ym init my-app            # 创建项目（零问答）
cd my-app
ym add com.google.guava:guava   # 添加依赖
ymc build                 # 编译（自动下载依赖）
ymc dev                   # 编译 + 运行 + 热重载
ymc test                  # 运行 JUnit 5 测试
```

## 命令

### 包管理（`ym`）

```bash
ym init [name]              # 创建新项目（-i 交互模式，-t 指定模板）
ym add <dep>                # 添加依赖（自动获取最新版本）
ym add <dep> --scope test   # 指定 scope（compile/runtime/provided/test）
ym remove <dep>             # 移除依赖（支持模糊匹配）
ym upgrade                  # 升级依赖（-i 交互式选择）
ym convert                  # 从 Maven/Gradle 迁移，生成 package.toml
ym tree                     # 依赖树（--depth, --flat, --dot, --reverse）
ym doctor                   # 环境诊断
ym info                     # 项目和环境信息
ym publish                  # 发布到 Maven 仓库（--dry-run 预览）
ym login                    # 登录 Maven 仓库
ym cache clean              # 清理依赖缓存（--pattern <p> 按模式匹配）
ym workspace list           # 列出工作空间所有包
ym workspace foreach -- CMD # 在每个包中执行命令（--parallel, --keep-going）
ym <script>                 # 执行 package.toml 中 [scripts] 定义的脚本
```

### 编译与运行（`ymc`）

```bash
ymc build                   # 编译（默认增量）
ymc build --release         # 构建生产用 fat JAR
ymc build --profile         # 显示各阶段耗时
ymc build --keep-going      # 模块失败时继续编译（workspace）
ymc dev                     # 开发模式：编译 + 运行 + 监听 + 热重载
ymc dev --debug             # 开发模式 + JDWP 调试器（端口 5005）
ymc test                    # 运行 JUnit 5 测试
ymc test --watch            # 监听模式：文件变更自动重跑
ymc test --coverage         # 生成 JaCoCo 覆盖率报告
ymc test --filter "MyTest"  # 过滤测试类/方法
ymc test --tag integration  # 按 JUnit @Tag 运行
ymc build --clean           # 清理构建产物后重新编译
ymc idea                    # 生成 IntelliJ IDEA 工程文件
```

## 配置

项目通过 `package.toml` 配置：

```toml
name = "my-app"
groupId = "com.example"
version = "1.0.0"
target = "21"                    # Java 版本

[dependencies]
"com.google.guava:guava" = "33.0.0-jre"
"org.springframework.boot:spring-boot-starter-web" = "3.4.0"
"org.junit.jupiter:junit-jupiter" = { version = "5.11.0", scope = "test" }

[scripts]
hello = "echo Hello from ym!"

[env]
SPRING_PROFILES_ACTIVE = "dev"
```

### 工作空间（Monorepo）

```toml
# 根 package.toml
name = "my-monorepo"
groupId = "com.example"
workspaces = ["modules/*"]

[dependencies]
"com.google.guava:guava" = "33.0.0-jre"

# 模块 package.toml — 从根继承
[dependencies]
"com.google.guava:guava" = { workspace = true }
"my-other-module" = { workspace = true }       # 模块间依赖
```

## 从 Gradle/Maven 迁移

```bash
cd my-existing-project
ym convert                   # 从 build.gradle 或 pom.xml 自动生成 package.toml
```

## 许可证

[MIT](../../../LICENSE)
