# JIT/CPU 修复与验证

本分支按 PR #5 的维护者反馈拆分，基于 `bd80650`。它只包含 CPU 转译、JIT/AOT 及其回归测试。

- 标量 FMA 根据 AVX/FMA 主机特性启用原生指令；非默认 FPCR 模式和异常状态通过受控的解释器路径处理。
- 操作数缓存、自循环的写穿寄存器缓存、连续条件选择及 NZCV 合并写回减少转译开销。
- 有限执行预算让长期链接的代码定期回到 dispatcher。每次退出保留完整架构状态，预算边界不再跳过看似已经无用的 NZCV 写回。
- Intel/AMD AVX 主机上的对齐 128 位成对访问使用单次访问；只读页、非对齐地址、寄存器别名和写入记录分别有检查。未宣称完整 LSE2 支持。
- AOT 版本为 25；主机能力和代码生成开关进入缓存标识。缓存大小和索引先验证，再发布代码；批量注册减少锁次数。

可用 `MACLATOR_JIT_FMA=0`、`MACLATOR_JIT_OPERANDS=0`、`MACLATOR_JIT_LOOP_CACHE=0`、`MACLATOR_JIT_REGISTER_BUDGET=0`、`MACLATOR_JIT_BUDGET_COLD=0`、`MACLATOR_JIT_CSEL_CHAIN=0`、`MACLATOR_JIT_PACKED_FLAGS=0` 比较保留的路径。开关在进程启动时读取，代码生成模式不同的缓存会被拒绝。

```sh
cargo test --locked --release --workspace -j 2
cargo build --locked --release -p maclator -j 2
target/release/maclator --selftest-jit budgetflags
target/release/maclator --selftest-jit branchcache 1200
target/release/maclator --selftest-jit fma 2000
target/release/maclator --selftest-jit operandcache 4000
target/release/maclator --selftest-jit fp-modes 2000
target/release/maclator --selftest-jit loopcache 1000
target/release/maclator --selftest-jit cselchain 2000
MACLATOR_JIT_REGISTER_BUDGET=0 target/release/maclator --selftest-jit budgetflags
MACLATOR_JIT_PACKED_FLAGS=0 target/release/maclator --selftest-jit cselchain 2000
python3 tests/check_cpu_pairs.py --runtime target/release/maclator \
  --sysroot /path/to/arm64-sysroot --output /tmp/maclator-cpu-pairs
```

这些命令检查正确性；本次拆分不提供新的性能成绩。此前组合版本的速度不能作为此独立分支的测量，95% 原生 x86 性能目标尚未达到。

信号/子进程/Xcode 和 Metal 分别提交。MacFlow、应用发现/原生启动入口/图标和后续实验保留在贡献者的 fork。此分支不依赖这些内容，也不修改本机安装版。
