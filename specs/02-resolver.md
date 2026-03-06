# 02 — 依赖解析

## 概述

ym 的依赖解析器负责将 `package.json` 中的 Maven 坐标解析为完整的传递依赖图，下载 JAR 到本地缓存，并通过锁文件保证可复现构建。

## 坐标格式

```
groupId:artifactId  →  "com.fasterxml.jackson.core:jackson-databind": "2.19.0"
```

- 版本前缀 `^` 和 `~` 会被自动剥离（当前不做语义化范围解析，直接使用精确版本）
- 锁文件记录实际解析版本 + SHA-256

## 解析流程

### 快速路径（锁文件命中）

```
package.json 中所有依赖都在 package-lock.json 中
  && 所有 JAR 文件在本地缓存中存在
  && SHA-256 校验通过
  → 直接返回 JAR 路径列表，零网络请求
```

### 慢速路径（需要网络）

```
1. 收集 package.json 中的直接依赖
2. BFS 遍历：
   a. 获取 POM 文件（本地缓存 → 网络下载）
   b. 解析 <parent>（最多 3 级深度）
   c. 收集 <properties> 和 <dependencyManagement>
   d. 解析 <dependencies>，跳过 scope=test/provided/system 和 optional
   e. 属性插值 ${property.name}
   f. 将传递依赖加入队列
3. 应用 exclusions 过滤
4. 应用 resolutions 版本覆盖
5. 并行下载所有 JAR（rayon par_iter）
6. SHA-256 校验
7. 写入 package-lock.json
```

### 仓库顺序

1. `registries` 中配置的自定义仓库（按配置顺序）
2. Maven Central: `https://repo1.maven.org/maven2`

支持 `.ym/credentials.json` 中的 Basic Auth 凭证。

## 缓存结构

```
.ym/cache/maven/
  com.fasterxml.jackson.core/
    jackson-databind/
      2.19.0/
        jackson-databind-2.19.0.jar
        jackson-databind-2.19.0.pom
```

## 锁文件格式 (package-lock.json)

```json
{
  "version": 1,
  "dependencies": {
    "com.fasterxml.jackson.core:jackson-databind:2.19.0": {
      "sha256": "abc123...",
      "dependencies": [
        "com.fasterxml.jackson.core:jackson-core:2.19.0",
        "com.fasterxml.jackson.core:jackson-annotations:2.19.0"
      ]
    }
  }
}
```

## 已知限制

| 问题 | 影响 | 严重性 |
|------|------|--------|
| POM 父级最多解析 3 级 | Spring Boot 继承链可能超限 | 高 |
| BFS 遍历串行执行 | 首次解析大项目慢 | 高 |
| 每个模块独立解析 | 工作空间重复 POM 请求 | 高 |
| 不支持 version range | `[1.0,2.0)` 等 Maven 范围不生效 | 中 |
| 不支持 classifier | `natives-linux` 等分类器 JAR 无法获取 | 中 |
| 不支持 BOM import | `<scope>import</scope>` 的 dependencyManagement 未处理 | 高 |
| 无 SNAPSHOT 支持 | 开发阶段依赖 | 低 |

## 优化路线图

### P0 — 工作空间级依赖合并（10-50x 提升）

```
当前：每个模块独立 resolve_and_download()
目标：收集所有模块声明 → 合并去重 → 一次性解析完整依赖图
```

### P1 — BOM import 支持

Spring Boot 项目核心需求。需解析 `<dependencyManagement>` 中 `<scope>import</scope>` 的 POM。

### P2 — POM 解析并行化

POM 下载是网络 I/O 密集，适合并行。当前 BFS 是串行的，改为分层并行：同一层级的 POM 并行获取。

### P3 — 父 POM 深度无限制

移除 3 级限制，改为循环检测（hash set 去重）。

### P4 — Version Range 支持

解析 Maven 版本范围语法 `[1.0,2.0)`，在候选版本中选择最高匹配。
