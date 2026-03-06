# 08 — JDK 管理

## 概述

ym 内置 JDK 扫描、自动下载和版本管理能力，目标是零配置开箱即用——项目声明 `target: "21"` 即可，ym 自动找到或下载对应 JDK。

## JDK 扫描

`scan_jdks()` 按优先级扫描：

| 顺序 | 来源 | 路径 |
|------|------|------|
| 1 | ym 管理 | `~/.ym/jdks/` |
| 2 | JAVA_HOME | 环境变量 |
| 3 | IntelliJ/JBR | JetBrains Toolbox + 独立安装 |
| 4 | SDKMAN | `~/.sdkman/candidates/java/` |
| 5 | Jabba | `~/.jabba/jdk/` |
| 6 | 系统 | `/usr/lib/jvm`, `/usr/java`, `/Library/Java/JavaVirtualMachines` |

每个 JDK 记录：
- vendor（Adoptium/GraalVM/JBR/...）
- version（主版本号提取）
- path
- source（来源类型）
- has_dcevm（是否支持增强热重载）

## JDK 自动下载

### 触发条件

`ensure_jdk()` 在以下情况自动下载：
1. JAVA_HOME 未设置
2. PATH 中无 javac
3. 缓存中无匹配版本
4. `jvm.autoDownload` 为 true

### 下载源

| 供应商 | API | 说明 |
|--------|-----|------|
| Adoptium (Temurin) | `https://api.adoptium.net/v3` | 默认 |
| JetBrains (JBR) | 交互式选择 | 支持 DCEVM |
| GraalVM | 交互式选择 | native-image |
| 自定义 URL | 用户输入 | 任意 tar.gz/zip |

### 下载流程

1. 检测 OS（linux/mac/windows）和架构（x64/aarch64）
2. 构建 Adoptium API URL
3. 下载 tar.gz/zip（带进度条）
4. 解压到 `~/.ym/jdks/{name}/`
5. 清理归档文件

### 交互式选择

`ym init` 交互模式提供：
- DEV JDK 选择（优先 JBR/DCEVM）
- PROD JDK 选择
- GraalVM 选择（可选，用于 native-image）

## JVM 参数管理

```json
{
  "jvmArgs": ["-Xmx512m", "-XX:+UseG1GC"],
  "env": {
    "DEV_JAVA_HOME": "~/.ym/jdks/jbr-25",
    "PROD_JAVA_HOME": "/usr/lib/jdk/graalvm-jdk-25"
  },
  "scripts": {
    "dev": "JAVA_HOME=$DEV_JAVA_HOME ymc dev",
    "build": "JAVA_HOME=$PROD_JAVA_HOME ymc build"
  }
}
```

通过 `env` + `scripts` 实现 DEV/PROD JDK 分离。

## 已知限制

- [ ] 版本匹配基于子字符串包含（可能误匹配）
- [ ] 仅支持 Adoptium API 自动下载
- [ ] `ym/0.1.0` User-Agent 硬编码
- [ ] 下载超时 300 秒不可配置
- [ ] 不支持代理（HTTP_PROXY）
- [ ] 不支持 .sdkmanrc 或 .java-version 文件

## 优化路线图

### P1 — 支持 .java-version 文件

读取项目根目录的 `.java-version` 文件自动选择 JDK。

### P2 — 多供应商 API 下载

支持直接从 GraalVM / Corretto / Zulu API 自动下载。

### P3 — 代理支持

读取 `HTTP_PROXY` / `HTTPS_PROXY` 环境变量。
