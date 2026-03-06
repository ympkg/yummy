# 06 — 测试

## 概述

ym 的测试系统支持 JUnit 5（JUnit Platform），提供测试发现、执行、过滤、覆盖率报告和监听模式。

## 命令

```bash
ymc test                                 # 运行所有测试
ymc test --filter "UserServiceTest"      # 按类名过滤
ymc test --verbose                       # 显示详细输出
ymc test --fail-fast                     # 首个失败即停止
ymc test --timeout 30                    # 每个测试类 30 秒超时
ymc test --coverage                      # 生成 JaCoCo 覆盖率报告
ymc test --list                          # 仅列出测试类
ymc test --watch                         # 监听模式：文件变化自动重跑
ymc test <module>                        # 工作空间：测试指定模块
```

## 执行流程

### 单项目模式

1. 编译主源码（`src/main/java` → `out/classes`）
2. 编译测试源码（`src/test/java` → `out/classes`，classpath 含主源码 + 所有依赖）
3. 发现测试类（扫描 `.java` 文件中的 `@Test` 或 `@org.junit` 注解）
4. 构建测试 classpath：`out/classes` + dependencies + devDependencies
5. 运行 JUnit Platform Console

### 测试发现

扫描测试目录中的 `.java` 文件，检查内容包含：
- `@Test`
- `@org.junit`

将文件路径转为类名：`src/test/java/com/example/FooTest.java` → `com.example.FooTest`

### JUnit Platform 执行

**优先方式：** 查找 `junit-platform-console-standalone` JAR：
```
java -jar junit-platform-console-standalone.jar
  --classpath {classpath}
  --scan-classpath
  --details verbose
```

**回退方式：** 直接调用 `org.junit.platform.console.ConsoleLauncher`

### 覆盖率（JaCoCo）

启用 `--coverage` 时：

1. 自动下载 JaCoCo agent（如果不存在）到 `.ym/tools/jacocoagent.jar`
2. 添加 JVM 参数：`-javaagent:jacocoagent.jar=destfile=out/coverage/jacoco.exec`
3. 测试完成后生成 `out/coverage/jacoco.exec`
4. 通过 JaCoCo CLI 生成 HTML 报告到 `out/coverage/html/`

### 工作空间模式

1. 构建目标模块及其所有传递依赖
2. 从所有依赖的 `out/classes/` 构建 classpath
3. 仅编译和运行目标模块的测试

## 已知限制

- [ ] 不支持 TestNG
- [ ] 不支持并行测试执行（JUnit 5 的 parallel execution config）
- [ ] `--filter` 仅按类名过滤，不支持方法级过滤
- [ ] 测试编译不使用增量编译（每次全量）
- [ ] 无测试报告输出（JUnit XML / HTML）

## 优化路线图

### P1 — 增量测试编译

复用增量编译指纹系统，仅重编译变更的测试文件。

### P2 — 测试报告

生成 JUnit XML 报告（兼容 CI 系统），可选 HTML 报告。

### P3 — 受影响测试检测

基于变更的源码文件，反向查找依赖这些类的测试，仅运行受影响的测试。
