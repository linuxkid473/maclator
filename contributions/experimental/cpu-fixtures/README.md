# CPU 候选的独立验证源

主运行时 JIT selftest 已包含预算、融合、循环、cached-store、有效地址、FP mode 和 GPR writeback 差分；这里补充主树之外的真实保存帧、tagged-memory 和 RET 真实字节 fixture。它们不被主安装流程执行。

- writeback/gpr-signals.c：ARM 保存 GPR 帧，修改 X21 并检查 sigreturn 后继续执行。历史八个 OFF/ON×预算进程修改 28 个 JIT 保存帧通过；AOT 另 11 个帧分别记录。
- writeback/scalar-vector-signals.c：ARM 保存 SIMD 帧，修改 V16。历史八模式 39 个帧通过；主标量候选九轮总体没有稳定性能收益，默认关闭。
- tagged-memory：103/117/131 例、256 种顶字节标签、内存/寄存器/向量/NZCV/写回状态；C 在 ARM 执行指令，在 x86 使用语义参考。Python model 独立计算预期 checksum，只是整数/字节 oracle，不执行 ARM；不等于所有 tag×NZCV 笛卡尔积、完整 ISA 或原子竞态。
- ret-index/check-index-model.py：全部 low17 bits×8 high patterns×3 base 的数学模型。
- ret-index/focused-ret-harness.c：实际 unmodified JIT dump body 配 fake Cpu/IBTC/hit-exit trampoline；历史 778272 次通过。参数为完整 code bytes、ARM word、guest PC、预算模式，不证明生产 guest、epoch/PAC 或所有失效并发。

这些固定工作负载不能把静态省指令数/检查点数换算成转译速度。C 需 clang/真实 macOS 和相应架构；信号源需 arm64，配显式 runtime/sysroot 运行。公开包不带 JIT dump 二进制或 Apple sysroot。Python oracle 会在同目录产生自己的结果 JSON；有的再次运行要求输出尚不存在，建议复制到临时目录运行；新结果与历史运行身份分别保存。

可先 clang -arch arm64 -O2 writeback/gpr-signals.c -o /absolute/new-output/gpr-signals-arm64，再通过显式 MACLATOR_JIT_GPR_WRITEBACK=1 和 MACLATOR_JIT_THRESHOLD=0 的 runtime --no-aot --sysroot /absolute/sysroot 运行它。数学模型可独立以 python3 ret-index/check-index-model.py 执行。
