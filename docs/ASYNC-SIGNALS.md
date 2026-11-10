# 异步信号、子进程与 Xcode 兼容性

本分支按 PR #5 的维护者反馈拆分，基于 `bd80650`。它只包含信号、Mach-O/子进程架构路由、线程/工作队列和 Xcode 组件路径修复。

宿主信号回调只发布原子状态；ARM handler 在 dispatcher 边界执行。实现包含 ARM siginfo/ucontext、全部寄存器、信号屏蔽、sigaltstack、嵌套、SA_RESETHAND、sigsuspend 和 SA_RESTART。同步硬件 fault 保持宿主行为，没有实现精确的宿主 PC 到客体状态恢复。

为使此 PR 可在未合并 CPU 优化时独立使用，JIT 增加最小的内存预算：每个 block 入口按最大 block 长度计费，最多经过 65,536 条指令的保守上界后回到 dispatcher。预算检查不插入 fused producer/consumer 中间，并保留 NZCV 写回。AOT 版本 23 拒绝旧的无预算缓存。两个 PR 共享这份最小预算基础提交；CPU PR 在同一祖先上替换为精确计费/寄存器预算和循环缓存，信号 PR 在 dispatcher 加入 poll。边界回归同时适应这两种计费方式。

Mach-O slice 读取涵盖 thin、FAT32/FAT64 和两种字节序，并检查文件边界。ARM-only 子进程通过 Maclator 重启；有 x86_64 slice 的子进程由宿主运行。`--no-aot`、sysroot 和可选组件 overlay 向 ARM 子进程传递。

线程创建等待 Mach port/TSD 初始化；工作队列从正确的参数读取请求数量。Xcode 的可选组件 overlay 和历史覆盖范围见 [XCODE.md](XCODE.md)。历史 Xcode 编辑器及 Swift 工程结果来自组合版本，本次独立分支的自动回归不能证明完整 Xcode、Simulator、设备部署或任意 XPC 服务可用。

```sh
cargo test --locked --release --workspace -j 2
cargo build --locked --release -p maclator -j 2
target/release/maclator --selftest-jit signal-budget
clang -arch arm64 -O2 -pthread tests/guest/signals_async.c \
  tests/guest/signals_registers.S -o /tmp/maclator-signals-arm64
codesign -f -s - /tmp/maclator-signals-arm64
python3 tests/check_guest_signals.py --runtime target/release/maclator \
  --guest /tmp/maclator-signals-arm64 --sysroot /path/to/arm64-sysroot \
  --output /tmp/maclator-signals-jit --deadline 8
python3 tests/check_guest_signals.py --runtime target/release/maclator \
  --guest /tmp/maclator-signals-arm64 --sysroot /path/to/arm64-sysroot \
  --output /tmp/maclator-signals-interp --interp --deadline 8
python3 tests/check_spawn_formats.py --runtime target/release/maclator \
  --sysroot /path/to/arm64-sysroot --output /tmp/maclator-spawn-formats
clang -arch arm64 -O2 tests/guest/spawn_options.c -o /tmp/maclator-spawn-options
python3 tests/check_spawn_options.py target/release/maclator \
  --guest /tmp/maclator-spawn-options --sysroot /path/to/arm64-sysroot
```

每个进程有超时，失败不会被计为兼容。测试不安装系统服务，不更改 Xcode/SDK，不分发 Apple 软件。MacFlow 与实验材料保留在贡献者 fork。
