# 独立 packed NZCV 贡献验证

基线：上游 `7a65c4af336f7258ee8c6d719f1d86a28c4587c6`。只修改 `emit.rs`、`aot.rs`、`selftest.rs` 和 `docs/DEBUGGING.md`；未导入本机其他优化，未安装、提交或推送。

## 实现

将保存 NZCV 的四次字节内存写入改成两次 16 位写入。ADD/SUB 使用 DL/DH、CL/CH 收集四个归一化标志；逻辑运算收集 NZ 并写入零 CV。只使用 SETcc/MOV，保留供分支/条件选择融合使用的宿主标志；不覆盖 RAX 运算结果。新增编译期字段连续性和对齐断言；原 `dead_flags` 行为保持不变。

默认开启，显式 `MACLATOR_JIT_PACKED_FLAGS=0` 可切换原路径。AOT 翻译版本升至 16，代码生成选项使用原本空闲的 bit 1，避免复用不同模式的缓存。

## 验证

`cargo test --locked --release --workspace -j 2`：2 个已有 Rust 测试全部通过。发布构建成功，保留上游既有编译警告。

`target/release/maclator --selftest-jit packedflags`：默认开启和显式关闭各运行 6,328 个确定性原生案例，零不一致、零跳过、零解释器回退。预期值由独立整数算术和条件公式计算，不调用参考解释器。覆盖：

- 32/64 位 ADD/SUB/ANDS/BICS、立即数和扩展寄存器，目标与源寄存器别名、XZR 丢弃结果。
- ADCS/SBCS 输入进位为 0/1，零、最大值、符号边界、进位和溢出边界。
- CCMP/CCMN 寄存器/立即数形式，条件真/假，以及所有 16 种 literal NZCV。
- ADD/SUB/逻辑生产者后接 B.cond/CSEL 的全部 16 条件编码，再接 ADC 消费已保存的进位。
- 全部通用/向量寄存器，归一化原始标志字节、PC、邻接 `_pad0` 哨兵和额外体系结构状态。

已有随机测试在两种模式分别运行：`dpimm 40000`、`dpreg 60000`、`fuse 24000`、`fuse2 24000`、`blocks 6400`；全部输出 `mismatch=0`。由于原随机生成器会产生非法/未实现编码，跳过数保留在日志中，不能把请求次数当成全部成功的原生覆盖。

## 失败保留与修正

首次编译失败来自新增测试对 `Box<Cpu>` 缺少解引用，已修正；首次确定性测试两种模式各有 1,920 个分支案例被测试代码错误地要求 `EXIT_DATA=0`，实际架构状态和 PC/NZCV 相同。上游分支退出返回可打补丁的链接槽地址，修正测试为验证其对齐及代码缓存范围后，两种模式全部通过。原失败日志未删除，也未当成通过。

## 性能范围

本次只做功能验证，没有测量独立上游移植的性能，不声称该 PR 提速 8% 或 9.45%。那些历史数字来自包含其他修改的运行时。

## 应用范围

这是便于维护者单独挑选的完整组件，仅适用于未修改的上游 7a65c4af336f7258ee8c6d719f1d86a28c4587c6。该独立移植使用 AOT16/bit1；本 PR 根目录为包含多项后续工作的 AOT22/bit11，已经有 packed NZCV，不应重复套用此补丁。此处不包含本机运行日志或二进制，只提供精确源补丁和上述验证摘要。

在独立的未修改上述 commit checkout 中使用 git apply --check，随后 cargo test --locked --release --workspace -j 2、cargo build --locked --release -p maclator -j 2，以及两种模式的 --selftest-jit packedflags。
