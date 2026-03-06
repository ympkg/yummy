# 05 — 运行与开发模式

## 概述

ym 提供三种运行模式：`ymc run`（一次性运行）、`ymc dev`（开发模式：编译+运行+监听+热重载）、以及 JDWP 远程调试支持。

## `ymc run` — 编译并运行

```bash
ymc run                                  # 运行默认主类
ymc run --class com.example.App          # 指定主类
ymc run --debug                          # 启用 JDWP 调试（端口 5005）
ymc run --debug --suspend                # 启动后挂起等待调试器
ymc run --debug-port 8000                # 自定义调试端口
ymc run -- arg1 arg2                     # 传递程序参数
```

### 主类解析优先级

1. `--class` 命令行参数
2. `package.json` 的 `main` 字段
3. 扫描源码中的 `public static void main` 方法
   - 0 个 → 报错
   - 1 个 → 自动选中
   - 多个 → 交互式选择（dialoguer::Select）

### JDWP 调试

```
-agentlib:jdwp=transport=dt_socket,server=y,suspend={y|n},address=*:{port}
```

默认端口：5005。`--suspend` 时 JVM 启动后暂停，等待调试器连接。

## `ymc dev` — 开发模式

```bash
ymc dev                                  # 默认模式
ymc dev --no-reload                      # 禁用热重载，变更时重启
ymc dev -- -Dspring.profiles.active=dev  # 传递 JVM 参数
```

### 完整生命周期

```
1. 执行 predev 脚本
2. 解析依赖
3. 首次编译
4. 查找主类
5. 构建 classpath
6. 配置 JVM 参数（jvmArgs + DCEVM + agent）
7. 启动 Java 进程
8. 进入监听循环：
   a. 等待文件变化（100ms 防抖）
   b. 增量编译变更文件
   c. 尝试热重载（L1→L2→L3）
   d. 热重载失败 → 重启进程
9. Ctrl+C → 执行 postdev 脚本
```

### 热重载三级策略

| 级别 | 策略 | 条件 | 速度 |
|------|------|------|------|
| L1 | HotSwap | 仅方法体变化 | ~50ms |
| L2 | ClassLoader | 类结构变化（新方法/字段） | ~200ms |
| L3 | Restart | ClassLoader 失败 | ~2-5s |

**ym-agent 通信协议：**
- 传输：TCP，127.0.0.1:{port}
- 格式：JSON
- 请求：`{"method":"reload","params":{"classDir":"...","classes":["com.example.Main"]}}`
- 响应：`{"success":true,"strategy":"HotSwap","timeMs":45}`

**DCEVM 检测：** 如果 JAVA_HOME 路径包含 `jbr` 或 `jetbrains`，自动添加 `-XX:+AllowEnhancedClassRedefinition`。

### 文件监听

- 引擎：`notify` crate，`RecursiveMode::Recursive`
- 监听目录：源码目录 + 资源目录
- 默认扩展名：`.java`（可通过 `hotReload.watchExtensions` 配置）
- 防抖：首个事件后等待 deadline，收集批量变更，去重

### ym-agent.jar

- 嵌入在 ym 二进制中（~7KB）
- 查找顺序：可执行文件目录 → `.ym/` → `~/.ym/`
- 首次使用自动提取到 `~/.ym/ym-agent.jar`

## 工作空间模式

```bash
ymc dev <module>                         # 开发指定模块
```

- 计算传递闭包，编译所有依赖模块
- 构建组合 classpath（所有依赖模块的 `out/classes/` + Maven JAR）
- 监听所有模块的源码目录
- 任何文件变化 → 重新编译所有模块 → 重启

## 已知限制

| 问题 | 影响 | 严重性 |
|------|------|--------|
| DCEVM 检测基于路径字符串 | 可能误检/漏检 | 低 |
| 工作空间模式变更触发全量重编译 | 大项目慢 | 高 |
| 热重载失败自动回退无配置 | 用户无法控制行为 | 低 |
| JDWP 地址 `*` 绑定所有接口 | 安全风险 | 中 |
| 多主类检测在 CI 中失败 | 非交互模式无法选择 | 中 |
| agent 仅支持 IPv4 本地 | 远程开发不可用 | 低 |

## 优化路线图

### P0 — 工作空间细粒度增量

当前：任意文件变化 → 重编译所有模块。
目标：根据变更文件归属的模块，仅重编译该模块及其下游依赖。

### P1 — DCEVM 检测改进

通过 `java -version` 输出检测 DCEVM/JBR，而非路径字符串。

### P2 — Spring Boot DevTools 集成

检测 `spring-boot-devtools` 依赖，自动配置 livereload 端口和 restart classloader。

### P3 — 端口冲突检测

启动前检查 JDWP 端口和 agent 端口是否被占用。
