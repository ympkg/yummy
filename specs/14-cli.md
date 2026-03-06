# 14 — CLI 架构

## 概述

ym 采用双二进制架构：`ym`（包管理器）和 `ymc`（编译器/运行时），由同一 Rust 二进制根据执行文件名分发。

## 二进制分发

```
ym    → 包管理命令（install, add, remove, upgrade, ...）
ymc   → 编译/运行命令（build, run, dev, test, ...）
```

**实现：** `main.rs` 中读取 `argv[0]` 的文件名，`"ymc"` 走 `ymc_main()`，否则走 `ym_main()`。

安装时复制同一二进制为两个文件名：
```bash
cp ym ymc    # Unix
cp ym.exe ymc.exe  # Windows
```

## ym 命令（包管理器）

```
ym init [name] [-t template] [-y]        # 初始化项目
ym install                               # 安装依赖
ym add <dep> [-D] [-W]                   # 添加依赖
ym remove <dep> [-D]                     # 移除依赖
ym upgrade [-i]                          # 升级依赖
ym outdated [--json]                     # 检查过时
ym lock [--check]                        # 重建锁文件
ym tree [--depth N] [--json] [--flat]    # 依赖树
ym deps [--json] [--outdated]            # 依赖列表
ym search <query> [--limit N]            # 搜索 Maven Central
ym why <dep>                             # 解释依赖来源
ym audit [--json]                        # 漏洞检查
ym verify                                # 校验完整性
ym dedupe [--dry-run]                    # 去重
ym pin <dep> [--unpin]                   # 固定版本
ym sources                               # 下载源码 JAR
ym config [key] [value]                  # 配置管理
ym cache {list|clean}                    # 缓存管理
ym env                                   # 环境信息
ym doctor [--fix]                        # 诊断
ym validate                              # 校验配置
ym license [--json]                      # 许可证
ym convert                               # Maven/Gradle 迁移
ym publish [--dry-run]                   # 发布
ym login                                 # 仓库登录
ym link [target] [--list] [--unlink]     # 本地链接
ym workspace {list|graph|changed|...}    # 工作空间
ym run <script> [-- args...]             # 运行脚本
ym completions <shell>                   # Shell 补全
```

## ymc 命令（编译器/运行时）

```
ymc build [target] [--release] [--watch] [-j N] [--profile] [-v] [--clean] [-o dir]
ymc dev [target] [--no-reload] [-- args...]
ymc run [target] [--class C] [--debug] [--debug-port P] [--suspend] [-- args...]
ymc test [target] [--watch] [--filter F] [-v] [--fail-fast] [--timeout N] [--coverage] [--list]
ymc check [target] [--strict]
ymc clean [--all]
ymc fmt [--check] [--diff]
ymc doc [target] [--open]
ymc bench [target] [--filter F]
ymc jar [target]
ymc exec [-- args...]
ymc classpath [target]
ymc hash [target]
ymc diff [target]
ymc size [target]
ymc graph [target] [--dot] [--reverse] [--depth N]
ymc watch [--ext E] [-- command...]
ymc rebuild [target] [--release]
ymc idea [target] [--sources]
ymc create <name> [-t template] [--deps]
```

## 外部子命令

`ym <unknown>` 会尝试作为脚本运行：
1. 加载 package.json
2. 在 `scripts` 中查找匹配名称
3. 找到 → 执行脚本
4. 未找到 → 报错

## Shell 补全

```bash
# Bash
eval "$(ym completions bash)"

# Zsh
ym completions zsh > ~/.zsh/completions/_ym

# Fish
ym completions fish | source

# PowerShell
ym completions powershell | Out-String | Invoke-Expression
```

## 设计决策

| 决策 | 原因 |
|------|------|
| 双二进制分离 | 职责清晰：ym 管依赖（如 yarn），ymc 管编译运行（如 vite） |
| 外部子命令作脚本 | `ym dev` 等于 `ym run dev`，减少输入 |
| clap derive | 类型安全的参数解析，自动 --help |
| 无 daemon | 简化架构，原生二进制启动足够快 |
