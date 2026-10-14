# MacFlow 0.1.0 实施与验收记录

日期：2026-10-13。状态：**可运行实验版，稳定版验收未通过**。

本报告保留历史本机验收结果。下文 `validation/` 文件名指当时的原始记录；这些可能含进程信息的本机快照未随源码贡献发布。当前源码的重新构建、测试结果和移植范围见仓库的 `contributions/提交验证.md` 与 `contributions/构建说明.md`，不能用历史安装记录替代新的构建证明。

## 已交付

本贡献中的独立 workspace 位于 `integration/macflow`，固定 Rust 1.99.0、Edition 2024 和 Cargo.lock。原生 Rust AppKit 应用已签名安装到 `/Applications/MacFlow.app`。界面、策略、用户服务、管理员助手、协议、诊断和 CLI 均为 Rust；Python 仅用于开发打包与有限验收。

包含五页界面、菜单栏、模式、观察/暂停、应用规则、最近聚合历史、五分钟细粒度导出、登录启动、受限 nice 租约、root 恢复日志、更新、回滚和卸载。状态机实现 CPU 两个五秒窗口、内存三个连续告警样本、十五秒恢复、未知和过期数据保护。普通用户服务每两秒续租，租约最长十秒；前台切换独立以 250 ms 检查撤销。

策略和协议禁止 unsafe；FFI 核对返回长度、所有权及结构布局。实机发现 libc 声明的 VM 统计尾部比 macOS 26.7.1 返回版本更长，已使用初始化存储与有效前缀长度校验。压缩、解压和换页增量记录真实 Mach 数据。内存准入由已验证的系统压力级别触发；压缩/换页阈值未校准，当前作为诊断数据。CPU 采样每秒执行，历史写入进入有容量上限的独立工作线程。

实机还发现服务主线程未运行 CFRunLoop，NSWorkspace 前台信息因此停留在启动值。已在主线程初始化 NSWorkspace，并使用有惰性保持源和 autorelease pool 的主事件循环；修复前的无效对照全部保留，未计入正式结果。

## 对原计划的实现调整

- XPC 采用公开 C XPC API 的 Rust 封装和有上限的版本化 JSON。动态 Rust Objective-C 协议不能提供 NSXPC 扩展 block 参数元数据，因此未采用 NSXPCConnection / NSData 载荷。
- 保留 objc2-service-management 的 SMAppService 注册绑定。本机 ad-hoc 版本采用经管理员授权的 launchd 兼容路径；root 助手和维护程序使用 root 保护的 Runtime 副本。尚无 Developer ID 签名或公证。
- 外部 GPU 仅监测，未经验证的温度和利用率保持未知。供电 ACPI 状态也保持未知，不参与控制。
- 反复 API 执行失败的优化规则会自动禁用。基于真实应用性能退步自动禁用规则的闭环尚未完成；失败性能门槛的功能保留实验状态。
- 自动 AOT 预热保持关闭。显式 `macflow aot` 通过进程锁每次运行一个，内存压力时拒绝新增可延迟工作。

这些调整和待验收项不能视为原计划的全部成熟产品目标已经完成。

## 恢复、信任和生命周期证据

root 特权文件：`/Library/Application Support/MacFlow/Runtime`，用户不可写。助手仅信任安装时认可的 agent CDHash；公开安装标记为 0644，信任和恢复日志为 0600。更新持有独占维护锁，拒绝新的 Apply/Renew，但继续允许撤销和到期恢复。

| 检查 | 实测结果 | 原始记录 |
| --- | --- | --- |
| nice 调整与显式撤销 | 专用 sleep 子进程 0 → 5 → 0 | `validation/helper-smoke.json` |
| 启动身份变化 | 拒绝 | 同上 |
| 超过 +5 的请求 | 拒绝 | 同上 |
| 保护进程 | 拒绝 | 同上 |
| 未授权签名客户端真实 XPC | 拒绝 | `validation/untrusted-helper.json` |
| 十秒不续租 | 约 10.08 秒观察到原值恢复 | `validation/helper-expiry.jsonl` |
| 活动租约时助手重启 | 约 0.11 秒恢复，早于租约到期 | `validation/helper-restart-verified.json` |
| 实际更新、回滚 | 已验证安装哈希及旧版恢复 | `validation/lifecycle-integration.json` |
| 活动租约时卸载 | 约 10.28 秒先恢复，再移除服务和应用 | 同上 |
| 再安装 | 助手连接、观察模式、无活动租约 | 同上 |

工作区 50 项测试通过（23 软件/通信/生命周期、19 策略、8 平台）。记录在 `validation/workspace-tests.log`。助手、用户服务和应用签名均在实际安装后核查。代码身份与系统调用之间的非原子间隙仍存在，未宣称消除全部 PID 复用风险。

## Maclator 部署

安装运行时 SHA256：`7722ec31b4ef0708a42da61456f2289ffe00b6c7e11b32592b26e5bf167daadb`。旧运行时、原生代理启动器、xcodebuild 和 swiftc 包装脚本分别备份；MacFlow 回滚不会一起替换 Maclator。

已移除固定 nice 10，后续启动正常继承优先级。Guest 线程启动、QoS 请求和 worker 复用映射真实宿主 QoS；保持依赖、overcommit、独立 GPU bridge、31-worker linker 和 Metal 顺序。GPU 动态 spin 通过原子快照读取，socket 通信和 JSON 处理在专门宿主线程，JIT 热路径不增加阻塞通信或分配。

15 项 Maclator 单元测试、线程/派生进程、六种 FAT、六轮 31-job dispatch_apply、真实 ARM linker 31-worker 路径及产物执行通过。原生和转译 Metal 绘制、反射、private texture 回读、计算回读、NSError 分离在默认、旧式、禁批处理及零 spin 下通过。记录位于 `validation/maclator-tests`。安装后 Xcode 27 版本查询、QoS、Metal 原生/转译回读再次通过，真实运行时已在用户 socket 注册并汇报统计，测试进程继承 nice 0。

这不代表全部 Xcode 调试、设备/模拟器部署、所有 ARM 图形窗口或所有 ARM 应用已经兼容。此前独立 CPU 转译基准的几何均值为原生的 40.53%，本次适配未重新认证其比例；95% 目标仍未通过。

## 九组正式 CPU 对照

原始结果：`validation/responsiveness-final/summary.json` 及每轮 JSON。十八个有限 CPU 负载轮次，九组观察/控制交错排序；每轮 30 线程、18 秒，前十二秒预热，后三秒测量原生探针队列延迟。观察轮原值保持，所有控制轮都捕获 nice 增加、助手连接、活动租约和 CPU 争用。

| 项目 | 本次结果 | 判断 |
| --- | --- | --- |
| 有效配对 | 9 / 9 | 有效 |
| 控制轮 p95 | 0.07978–0.10205 ms | 低于 50 ms |
| 控制轮 p99 | 0.11698–0.15606 ms | 低于 100 ms |
| 每组 p95 改善 ≥20% | 没有一组达到 | **未通过** |
| 后台等工作量耗时增长推算 | 中位数约 -0.28%，最差约 +0.57% | 仅本 CPU fixture 的吞吐推算 |
| 回调错误、跳过间隔 | 所有正式轮次均为零 | 探针范围内通过 |

该探针在高 CPU 负载下本来已接近 0.1 ms，调度前后差异不足以证明改善；部分轮次更慢。不能用这些结果声称系统全局更流畅。吞吐推算不能替代 Xcode 等真实应用的固定工作量耗时测试。无争用的 ≤3% 退步门槛、普通应用交互和经过验证的呈现延迟尚未完成。

脚本已恢复原模式、观察和暂停状态，并保护专用 fixture 路径；最终默认仅观察，未推荐自动启用实际控制。EFI、ACPI、驱动和永不休眠设置均未修改。

## 仍待完成的稳定版门槛

空闲常驻测量见 `validation/idle-60s.json`，该文件生成后补充本报告。Xcode 编辑/完整构建对照、浏览器/聊天、音视频、真实前后台负载、内存/磁盘专项、无争用退步、实际应用固定工作量代价、24 小时压力及七天日常验证仍需验收。性能退步反馈的自动规则停用、更多因果诊断及通用 ARM 应用覆盖也仍在实验范围。

代码和安装可运行，不等于上述发布门槛已经通过；当前应以观察和诊断为默认用途。
