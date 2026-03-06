# 13 — 脚本与生命周期

## 概述

ym 支持 npm 风格的自定义脚本和生命周期钩子。

## 自定义脚本

```json
{
  "scripts": {
    "dev": "JAVA_HOME=$DEV_JAVA_HOME ymc dev",
    "build": "JAVA_HOME=$PROD_JAVA_HOME ymc build",
    "test": "ymc test",
    "start": "ymc run",
    "docker:build": "docker build -t $ARTIFACT .",
    "native": "ymc build --release && $GRAALVM_HOME/bin/native-image -jar out/$ARTIFACT.jar"
  }
}
```

### 运行方式

```bash
ym run dev                               # 显式运行
ym dev                                   # 快捷方式（外部子命令匹配）
```

### 执行机制

- Shell：Windows `cmd /C`，Unix `sh -c`
- 工作目录：项目根目录（package.json 所在目录）
- 环境变量：`env` 字段中的变量自动注入，支持 `~/` 展开为 `$HOME`

## 生命周期钩子

| 钩子 | 触发时机 |
|------|---------|
| `preinit` | `ym init` 之前 |
| `postinit` | `ym init` 之后 |
| `prebuild` | `ymc build` 之前 |
| `postbuild` | `ymc build` 之后 |
| `predev` | `ymc dev` 之前 |
| `postdev` | `ymc dev` 之后 |
| `pretest` | `ymc test` 之前 |
| `posttest` | `ymc test` 之后 |
| `prepublish` | `ym publish` 之前 |
| `postpublish` | `ym publish` 之后 |

钩子定义在 `scripts` 中，命名即触发。

## 已知限制

- [ ] 脚本命令不支持管道和复合命令（传给 shell 执行，实际支持取决于 shell）
- [ ] 无超时控制
- [ ] 无并行脚本执行
- [ ] 环境变量展开仅支持 `~/` 前缀，不支持 `$VAR` 交叉引用

## 优化路线图

### P1 — 脚本参数传递

```bash
ym run build -- --release                # 将 --release 传给脚本
```

### P2 — 脚本组合

```json
{
  "scripts": {
    "ci": "ym run build && ym run test && ym run lint"
  }
}
```
