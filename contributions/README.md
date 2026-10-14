# Maclator 完整贡献与验证范围

本贡献整理了在 Intel/AMD x86_64 macOS 上运行 ARM64 应用时完成的转译优化、兼容性修复、应用入口、图标、Metal 桥、MacFlow 集成，以及仍需研究的实验。上游基线为 `7a65c4af336f7258ee8c6d719f1d86a28c4587c6`。这是可供维护者按模块审查、拆分和选择的完整贡献。

运行时代码来自经过验证的最后一份冻结源码；已采用的功能保留原有默认值，后续实验保留显式开关。独立的较早原型及 GPU 研究材料放在 [experimental](experimental/README.md)，不会自动替换主运行时。源代码布局和 MacFlow 依赖的迁移见 [构建说明](构建说明.md)。

## 贡献目录

| 领域 | 主要实现 | 内容 |
| --- | --- | --- |
| CPU 转译 | `crates/maclator/src/jit/`、`crates/core/src/` | 原生 FMA、操作数读取缓存、循环寄存器缓存、执行预算、冷退出、条件选择链、NZCV 合并写回、成对访问、浮点模式处理 |
| JIT/AOT 正确性 | `jit/`、`aot.rs` | 预算边界保存完整状态、模式标识和缓存隔离、缓存索引验证、差分测试、指令归因 |
| ARM 应用兼容 | `macho.rs`、`slices.rs`、`spawn.rs`、`signals.rs`、`syscalls.rs`、`threads.rs`、`workq.rs`、`paths.rs` | Mach-O 边界检查、thin/FAT32/FAT64 子进程路由、异步信号帧、线程/工作队列、路径覆盖及 Xcode 启动和构建适配 |
| 应用启动与图标 | `integration/`、`app_icon.rs` | 用户级应用发现、原生应用入口、ARM 启动转发、正常 Dock 图标及子进程环境清理 |
| Metal 桥 | `gpu/`、`gpu.rs`、`tests/gpu/` | pipeline reflection、NSError 输出参数、绘制与计算回读；额外呈现实验单独保存 |
| MacFlow | `integration/macflow/`、`macflow.rs` | 进程资源管理组件及 Maclator 适配器、协议、构建与生命周期测试 |
| 回归工具 | `tests/`、`bench/`、`integration/test*` | 指令差分、信号、进程启动、Foundation/C++/原子操作、GPU、应用入口与基准驱动 |

## 默认开关

以下是源码当前默认行为，环境变量在进程启动时读取。硬件指令优化还会检查实际 CPU 特性。

| 功能 | 环境变量 | 默认 |
| --- | --- | --- |
| 原生 FMA | `MACLATOR_JIT_FMA` | 开启；要求 AVX/FMA |
| 操作数读取缓存 | `MACLATOR_JIT_OPERANDS` | 开启 |
| 循环寄存器缓存 | `MACLATOR_JIT_LOOP_CACHE` | 开启 |
| 寄存器执行预算 | `MACLATOR_JIT_REGISTER_BUDGET` | 开启 |
| 冷预算退出 | `MACLATOR_JIT_BUDGET_COLD` | 开启 |
| 条件选择链 | `MACLATOR_JIT_CSEL_CHAIN` | 开启 |
| 完整 NZCV 合并写回 | `MACLATOR_JIT_PACKED_FLAGS` | 开启 |
| 相邻成对寄存器快路径 | `MACLATOR_JIT_PAIR_FAST` | 关闭；显式 `1` 开启 |
| 固定缓存有效地址 | `MACLATOR_JIT_EFFECTIVE_ADDRESS` | 关闭；显式 `1` 开启 |
| 缓存寄存器直接存储 | `MACLATOR_JIT_CACHED_STORE` | 关闭；显式 `1` 开启 |
| BMI2 TBI 归一化 | `MACLATOR_JIT_TBI_BMI2` | 关闭；显式 `1` 且有 BMI2 |
| 非回写 SIMD 结构加载 | `MACLATOR_JIT_STRUCTURE_LOADS` | 关闭；显式 `1` 且有 SSSE3 |
| RET 索引路径 | `MACLATOR_JIT_RET_INDEX` | 关闭；显式 `1` 开启 |
| 标量 SIMD 延迟写回 | `MACLATOR_JIT_SCALAR_WRITEBACK` | 关闭；显式 `1` 开启 |
| 纯整数 GPR 延迟写回 | `MACLATOR_JIT_GPR_WRITEBACK` | 关闭；显式 `1` 开启 |

已开启功能可用显式 `0` 关闭。AOT 版本为 22，代码生成选择参与缓存标识；改变开关后旧模式的缓存会被拒绝。私有 `MACLATOR_*` 环境变量可能从客体环境过滤，应用根进程的实验开关不能代表其全部 ARM 子进程。具体限制见 [JIT 调试说明](../docs/DEBUGGING.md)。

## 验证与性能结论

本项目的历史本机验证包括 Xcode 项目编辑器和真实 Swift ARM64 工程构建、应用入口与图标、ARM 子进程路由、异步信号、Metal 绘制/计算回读，以及多种 JIT/AOT 差分测试。它们证明的是明确列出的用例，不能换算为所有 ARM 应用的兼容率。提交目录重新构建的结果记录在 [提交验证](提交验证.md)。

最近一次已部署的 NZCV 优化，在同一套九轮测试中相对上一份已修改运行时，配对中位速度比为 `1.094525×`，等效耗时减少 `8.64%`。同一 binary 关闭/开启该开关的配对中位速度比为 `1.084959×`。这些对照的基线已经含有此前的修改，因此不作为“相对未经修改上游的累计提升”来使用。不同时间、不同版本的提升百分比不能相加。

已安装运行时在七项 CPU 基准中的总吞吐约为同硬件原生 x86 的 54%～56%；95% 的目标尚未达到。这些测量不是当前重新打包目录的新性能分数，也不代表 Xcode 的整体速度。

后续实验存在整体收益不稳定、部分内核回退或没有命中基准热点的情况，所以维持默认关闭。纯整数 GPR 延迟写回已通过范围内的全状态、信号和 AOT 验证，但两个七内核基准的主程序计划均没有接受该快路径，没有给它生成新的正式性能分数。其异步调试转储还允许不同寄存器具有不同新鲜度，尚不提供精确的同步 fault 恢复。AOT 与运行时 verifier 的组合也有既有范围限制。

GPU 呈现实验的工作线程返回、回调、帧时间戳及回读结果，不能独立证明屏幕内容正确或高帧率；对应材料保留实际失败与解析器修正，并在实验目录中说明。

## 复现入口

```sh
cargo test --locked --release --workspace -j 2
cargo build --locked --release -p maclator -j 2
target/release/maclator --selftest-jit budgetflags
target/release/maclator --selftest-jit fma 24000
target/release/maclator --selftest-jit operandcache 24000
target/release/maclator --selftest-jit fp-modes 10000
```

更多测试使用用户自行准备的 ARM64 sysroot，命令和参数见 `docs/DEBUGGING.md`、`tests/compatibility_matrix/README.md` 与 `integration/` 下的测试工具。构建和测试工具不自动安装 EFI、驱动或更改启动配置。提交包含源码、测试与文档，Apple sysroot、SDK、Xcode 本体和本机应用产物仍由使用者在本地准备。
