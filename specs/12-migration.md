# 12 — 迁移

## 概述

`ym convert`（别名 `ym migrate`）自动将 Maven 或 Gradle 项目转为 ym 格式。

## 命令

```bash
ym convert                               # 自动检测 pom.xml 或 build.gradle
```

## 检测逻辑

1. 当前目录有 `pom.xml` → Maven 迁移
2. 当前目录有 `build.gradle` 或 `build.gradle.kts` → Gradle 迁移
3. 都没有 → 报错
4. 已有 `package.json` → 拒绝覆盖

## Maven 迁移

解析 `pom.xml`：

| Maven 字段 | package.json 字段 |
|------------|-------------------|
| `<groupId>` + `<artifactId>` | `name` |
| `<version>` | `version` |
| `<description>` | `description` |
| `<properties><maven.compiler.source>` | `target` |
| `<dependencies>` (scope!=test) | `dependencies` |
| `<dependencies>` (scope=test) | `devDependencies` |

## Gradle 迁移

解析 `build.gradle` / `build.gradle.kts`：

| Gradle 配置 | package.json 字段 |
|-------------|-------------------|
| `group` + `archivesBaseName` | `name` |
| `version` | `version` |
| `sourceCompatibility` | `target` |
| `implementation`, `api` | `dependencies` |
| `testImplementation` | `devDependencies` |

**解析方式：** 正则表达式（非 AST），支持常见格式但不保证 100% 覆盖。

## 已知限制

- [ ] Gradle 解析基于正则，复杂脚本可能遗漏
- [ ] 不支持 Gradle Version Catalog (`libs.versions.toml`)
- [ ] 不迁移插件配置（shade/spring-boot/annotation-processing）
- [ ] 不迁移多模块 Maven/Gradle 项目为 workspace
- [ ] 不迁移 Maven profiles

## 优化路线图

### P1 — 多模块迁移

检测 Maven `<modules>` 或 Gradle `settings.gradle` 的 `include`，自动生成 workspace 结构。

### P2 — Gradle Version Catalog

解析 `gradle/libs.versions.toml` 提取依赖声明。
