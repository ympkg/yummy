# 10 — 发布与分发

## 概述

ym 支持将 Java 库发布到 Maven 仓库，以及通过 `link` 命令进行本地跨项目开发。

## `ym publish`

```bash
ym publish                               # 发布到默认仓库
ym publish --dry-run                     # 模拟发布（不上传）
```

### 发布流程

1. 检查 `"private": true` → 拒绝发布
2. 执行 `prepublish` 脚本
3. 编译项目
4. 打包 JAR
5. 生成 POM 文件
6. 上传到 Maven 仓库（JAR + POM + SHA-256）
7. 执行 `postpublish` 脚本

### POM 生成

从 `package.json` 映射：
- `name` → `artifactId`
- `version` → `version`
- `description` → `description`
- `license` → `license`
- `dependencies` → `<dependencies>`

## `ym login`

```bash
ym login                                 # 交互式输入仓库 URL + 凭证
```

凭证存储在 `.ym/credentials.json`：
```json
{
  "https://maven.example.com": {
    "username": "...",
    "password": "..."
  }
}
```

## `ym link`

本地跨项目开发（类似 `npm link`）。

```bash
# 在库项目中注册
cd my-lib && ym link

# 在消费项目中引用
cd my-app && ym link my-lib

# 查看已链接
ym link --list

# 解除链接
ym link --unlink my-lib
```

链接机制：创建符号链接到目标项目的 `out/classes/`，添加到 classpath。

## 已知限制

- [ ] 不支持 GPG 签名（Maven Central 要求）
- [ ] 不支持 Javadoc JAR 上传
- [ ] 不支持 Sources JAR 上传
- [ ] POM 生成不含 parent/dependencyManagement
- [ ] link 不支持传递依赖

## 优化路线图

### P1 — Maven Central 发布

支持完整的 Maven Central 发布流程：GPG 签名、Javadoc JAR、Sources JAR、Staging。

### P2 — GitHub Packages 集成

自动检测 GitHub Actions 环境，使用 GITHUB_TOKEN 发布。
