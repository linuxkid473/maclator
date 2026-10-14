# 构建、验证和发布门槛

MacFlow 0.1.0 是实验实现。构建成功和单元测试通过不代表整机性能、驱动兼容或长期稳定性已验收。运行中的应用、服务、助手和命令行均由 Rust 编译；`scripts/package.py` 仅负责构建产物打包。

## 构建与签名

在项目根目录运行：

```sh
cargo test --workspace --locked
cargo build --release --workspace --locked
python3 scripts/package.py
codesign --verify --deep --strict dist/MacFlow.app
```

打包器只接受原生 x86_64 Mach-O release 二进制，生成四个独立签名的程序（界面源文件 `macflow-ui` 打包为 `MacFlowUI`，另有 `macflow`、`macflow-agent`、`macflow-helper`；名称避免 APFS 大小写冲突）、应用签名及嵌入的 SMAppService LaunchAgent/LaunchDaemon。输出在 `dist/MacFlow.app`，构建摘要在 `dist/MacFlow.build.json`。打包不安装，不注册服务，不修改 EFI、ACPI 或驱动。构建摘要中的哈希是完成签名后的二进制哈希。

第一次启动默认观察。服务安装、注册及本机管理员授权由 Rust 命令行和系统授权界面处理；不得把用户密码写入脚本或报告。先验证 `macflow status`、导出和暂停恢复，再启用实际调度。

## 自动化检查

- 策略：两个 CPU 窗口、三个内存警告样本、十五秒恢复、过期数据、前台切换、保护应用、未知任务和并发建议。
- 协议：未知版本、未知字段、超过 64 KiB 的数据、响应编号与版本不匹配、运行时身份失效。
- XPC：不存在的服务必须及时返回；有效签名双方完成真实往返；签名要求不匹配必须拒绝；超时不自动重试写操作。
- 租约：身份变化、外部优先级变更、十秒断联、助手重启、恢复失败、已有日志与跨重启记录。
- Rust FFI：对象生命周期、回调异常隔离、主线程界面、句柄释放、无锁阻塞调用。
- Maclator：线程 QoS 映射与复用、同步依赖、31-worker linker、Metal 提交顺序、独立桥接线程、热路径无新阻塞和分配。
- 界面：五页内容、未知指标、超过七秒无服务更新时显示过期、规则操作错误、模式切换、导出、退出先暂停，关闭窗口保留菜单服务。

XPC 使用公开 C API 的 Rust FFI：动态 Rust Objective-C 协议不能提供 NSXPC 所需的扩展 block 参数元数据。消息为有边界的 XPC data JSON，签名要求由 XPC 框架验证，身份来自内核，不依赖客户端提供的 UID/PID。

## 本机对照测试

固定供电、刷新率、缓存、应用版本和工作负载，交错执行至少九轮关闭/开启。记录每轮原始结果和测试配置，不能只保留最好的结果。

| 场景 | 检查内容 |
| --- | --- |
| Xcode 编辑、全新构建与窗口切换 | 输入响应、构建耗时、依赖完整性 |
| 浏览器、微信、音乐与后台计算 | 滚动、输入、音视频连续性、前台保护 |
| 原生和 ARM 图形测试 | Metal 填充、复制、回读、经过验证的呈现回调 |
| 内存压力、磁盘复制 | 准入控制、恢复与数据完整性 |
| 服务、助手和运行时断联 | 租约恢复、前台撤销、无误改系统进程 |
| 更新、回滚与卸载 | 代码信任、原状态恢复、服务清理、Maclator 保持独立 |

专用交互探针可以测量该探针自己的事件队列延迟；该数据不是全局 FPS，也不是物理输入到显示延迟。呈现测量只有在自定义驱动的回调已验证时才纳入结果。未知 GPU 利用率和温度不得填零。

## 稳定版验收

| 项目 | 要求 |
| --- | --- |
| 专用交互延迟 | p95 ≤ 50 ms，p99 ≤ 100 ms |
| 已存在争用的场景 | p95 延迟改善 ≥ 20%，逐场景报告 |
| 后台任务耗时 | 中位数增加 ≤ 15% |
| 无争用场景 | 性能退步 ≤ 3% |
| 常驻组件开销 | 空闲平均 CPU < 单核 1%，合计内存 ≤ 150 MB |
| 故障恢复 | 十秒租约到期开始恢复，无保护进程误调节 |
| 持续运行 | 二十四小时压力、七天日常，无新增崩溃、黑屏、数据损坏或依赖死锁 |

未完成的门槛显示“待验证”；未通过的控制保持实验状态或禁用。Maclator 95% 原生性能目标必须另行使用同硬件原生 x86 基准验证。

## Rust 交互队列探针

```sh
cargo run --release --locked -p macflow --example interaction_probe -- --duration 30 --label observation --json /tmp/macflow-observation.json
cargo run --release --locked -p macflow --example interaction_probe -- --duration 30 --label control --json /tmp/macflow-control.json
```

探针不切换管理器模式；每轮开始前明确记录观察／管理状态，并在另一终端启动相同编译负载。每个文件保留原始延迟样本、跳过的采样间隔和分位数。窗口启动阶段也被记录，比较时必须保持启动步骤一致，不删掉高延迟样本。该探针不替代 Xcode、聊天、媒体和 Metal 的专项验收。

本机采用 ad-hoc 签名及精确 CDHash 信任，尚未进行 Developer ID 签名或公证。SMAppService/launchd 兼容性和管理员助手恢复必须在真实安装后验证；成功生成 `.app` 不代表这些检查已经通过。
