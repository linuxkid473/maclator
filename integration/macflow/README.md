# MacFlow

面向这台 x86_64 macOS 26 黑苹果的 Rust 资源管理软件。版本 **0.1.0 实验版**：原生 AppKit 界面、用户后台服务、最小权限管理员助手、CLI、恢复日志和 Maclator 合作接口已实现。首次运行及系统、驱动、Maclator 版本变化后进入观察模式。

软件控制已确认后台计算任务的资源争用，无法保证任意负载下零卡顿。驱动死锁和应用错误需要专项处理。Maclator 的 95% 原生性能目标独立验收，当前尚未达到。EFI、ACPI、内核驱动、NVIDIA 库和永不休眠设置不由本软件修改。

## 使用

打开 `/Applications/MacFlow.app`。原生菜单栏和窗口包含总览、应用、性能、诊断、设置五页。未知 GPU 利用率、温度和无法可靠识别的供电状态保留为未知。观察模式只采样，用户可通过设置开启实验调度。

命令行位于 `/Applications/MacFlow.app/Contents/MacOS/macflow`：

```sh
macflow status --json
macflow mode auto                 # auto / smooth / performance
macflow observe on               # on / off
macflow pause
macflow resume
macflow rule background path:/absolute/path/to/compiler
macflow rule protect path:/absolute/path/to/application
macflow export
macflow build -project Example.xcodeproj -scheme Example
macflow aot /absolute/path/to/arm64-executable
```

上面的 `macflow` 可替换为完整路径。规则还可使用 `bundle:com.example.app`；身份不明、系统服务、媒体和前台依赖默认保护。重新设置规则可解除该规则因反复执行失败而产生的禁用状态。`build` 为后续启动的 Xcode 构建传入并发建议；已有构建不会强制调整线程数。`aot` 只允许运行一个独立预编译任务，内存压力阻止新的可延迟任务。GUI 继续使用 `--no-aot`，自动预热保持关闭。

| 模式 | 正常 / 争用构建并发 | 前台 / 后台 GPU spin |
| --- | --- | --- |
| 自动 | 12 / 8 | 5 / 0 µs |
| 流畅优先 | 8 / 4 | 5 / 0 µs |
| 性能优先 | 16 / 12 | 20 / 5 µs |

压力或 GPU 异常时 spin 降为零。此操作只作用于接入 Maclator 的每线程桥接，原生应用 GPU 保持监测。原始 guest 线程、同步顺序、overcommit 和 31-worker linker 保留。

## 恢复与信任

CPU 超过 85% 且有可管理后台计算任务，连续两个五秒窗口才产生争用控制。nice 每次最多增加 5，上限 10；前台切换后撤销相关限制。内存警告连续三个样本触发保护，持续正常十五秒后恢复。数据失效时停止控制。

助手验证签名、UID、启动身份、boot ID 和保护规则，修改前持久保存原值、最后写入值及期限。用户服务每两秒续租，最长十秒；控制器失联后助手撤销自己的写入。恢复时发现其他程序改值就停止接管。助手重新启动先处理日志，跨系统重启丢弃旧 PID。PID 身份验证降低复用风险，系统调用与身份核对仍存在不可消除的非原子间隙。

本机版本使用 ad-hoc 签名和精确 CDHash，尚无 Developer ID / 公证。根服务和授权维护程序存放于 root 保护目录，助手仅信任安装时认可的用户服务。保留 SMAppService 的 Rust 注册绑定；本机 ad-hoc 安装采用经管理员授权的 launchd 兼容路径，用户服务在登录时启动。XPC 使用公开 C API 的 Rust FFI 和有上限的版本化 JSON：动态 Rust Objective-C 协议无法表达 NSXPC 所需的扩展 block 元数据，因此未使用 NSXPCConnection。策略与协议 crate 禁止 unsafe，系统调用封装在平台及通信边界。

## 构建、更新和卸载

Rust 1.99.0 / Edition 2024，`Cargo.lock` 固定依赖。

```sh
cargo fmt --all
cargo test --workspace --locked
cargo build --release --workspace --locked
python3 scripts/package.py
dist/MacFlow.app/Contents/MacOS/macflow install dist/MacFlow.app
```

最后一步调用本机管理员授权流程。Python 只用于开发打包和验收；安装后的产品组件均是 Rust 二进制。更新同样使用 `install`，先恢复控制并备份旧应用。`macflow rollback` 恢复独立的上一份 MacFlow 安装；`macflow uninstall` 先撤销控制再删除服务和应用，保留本地用户诊断数据供检查。恢复日志不为空或无法验证时维护操作停止，避免丢失恢复责任。

本贡献包含 [Maclator 适配源码](../../crates/maclator/src/macflow.rs) 和配套协议，构建不依赖仓库外的另一份工作目录。历史开发目录中的 `validation/maclator-adapter.patch`、`validation/maclator-before` 和原始本机记录不随源码发布。历史部署工具要求它明确检查的包装脚本基线；它在本机执行时会将运行时、代理启动器及两个编译包装脚本备份到 `~/Library/Application Support/Maclator/Backups/macflow-adapter-*`。已有相应备份时可运行回滚工具：

```sh
python3 scripts/deploy_maclator.py --restore '/absolute/path/to/macflow-adapter-backup'
```

它只恢复运行时和代理入口，不改原 ARM 应用、sysroot 或驱动；正在运行的代理下一次退出后更新。

## 数据与验收

用户数据：`~/Library/Application Support/MacFlow`。保留五分钟细粒度环形采样和七天聚合历史；历史 450 MiB、导出 40 MiB，root 恢复文件独立保留。导出含本机进程信息，默认只存本地。

测试方式和门槛见 [docs/TESTING.md](docs/TESTING.md)、[docs/CPU_COMPARISON.md](docs/CPU_COMPARISON.md)；本次结果见 [docs/IMPLEMENTATION_REPORT.md](docs/IMPLEMENTATION_REPORT.md)。回调探针只测自身主线程队列延迟，不代表全局 FPS 或物理输入到显示延迟。

稳定版仍须通过 Xcode、浏览器/聊天、音视频、原生与 ARM 图形、内存/磁盘专项对照，24 小时压力和七天日常验证。未通过的能力保留实验状态，不能由编译、短时启动或单项测试推定整机成熟。
