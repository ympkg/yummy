# 03 — 包管理命令

## 概述

ym 的包管理命令对标 npm/yarn，提供依赖的增删查改生命周期管理。

## 命令清单

### `ym install`

安装 `package.json` 中所有依赖到本地缓存。

```bash
ym install
```

- 调用 `resolve_and_download()` 解析并下载所有 dependencies + devDependencies
- 生成/更新 `package-lock.json`
- 快速路径：锁文件 + 缓存都在 → 秒级完成

### `ym add <dep>`

添加依赖到 `package.json`。

```bash
ym add com.google.guava:guava           # 自动获取最新版本
ym add com.google.guava:guava@33.0      # 指定版本
ym add guava                            # 搜索 Maven Central 并选择
ym add -D junit-jupiter                 # 添加到 devDependencies
ym add -W core                          # 添加工作空间模块间依赖
```

**流程：**
1. 解析坐标（支持简短名称 → Maven Central 搜索）
2. 无版本 → 查询最新 release 版本
3. 写入 `package.json` 的 dependencies/devDependencies
4. 调用 `resolve_and_download()` 更新锁文件

### `ym remove <dep>`

从 `package.json` 移除依赖。

```bash
ym remove com.google.guava:guava
ym remove -D junit-jupiter              # 从 devDependencies 移除
```

### `ym upgrade`

升级依赖到最新版本。

```bash
ym upgrade                              # 升级所有
ym upgrade -i                           # 交互式选择
```

- 查询每个依赖的最新版本
- 对比当前版本，显示可升级列表
- 交互模式：checkbox 多选

### `ym outdated`

检查过时依赖（不修改，仅报告）。

```bash
ym outdated
ym outdated --json
```

输出格式：

```
Package                                    Current   Latest
com.fasterxml.jackson.core:jackson-databind  2.17.0    2.19.0
```

### `ym lock`

重新生成锁文件。

```bash
ym lock                                 # 重新解析并生成
ym lock --check                         # CI 模式：检查锁文件是否最新
```

`--check` 在 CI 中使用，锁文件过期则 exit 1。

### `ym dedupe`

去重锁文件中的依赖版本。

```bash
ym dedupe
ym dedupe --dry-run                     # 仅显示，不修改
```

### `ym pin <dep>`

固定依赖版本，防止 `ym upgrade` 升级。

```bash
ym pin com.google.guava:guava
ym pin --unpin com.google.guava:guava
```

### `ym search <query>`

搜索 Maven Central。

```bash
ym search jackson
ym search jackson --limit 20
```

### `ym sources`

下载所有依赖的 `-sources.jar`（用于 IDE 调试跳转）。

```bash
ym sources
```

## 待改进

- [ ] `ym add` 支持从 URL 安装（本地 JAR 或远程 JAR）
- [ ] `ym add` 支持 Git 仓库依赖
- [ ] `ym upgrade` 支持 semver 范围约束（仅升级 minor/patch）
- [ ] `ym cache` 支持离线模式（`--offline` 标志）
- [ ] `ym install --frozen` 严格模式（锁文件不匹配直接失败）
