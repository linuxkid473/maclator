# 未默认启用的研究贡献

当前主运行时已经包含有效地址、cached store、BMI2/TBI、SIMD structure load、RET index、标量 SIMD 写回和整数 GPR 写回候选，各自显式开启、默认关闭；此处不重复导出整个 CPU 源树。

本目录贡献主树未包含的三个历史 CPU 分支、独立同步 fault/ABI/startup 研究、默认关闭的自动 local-present 桥，以及窗口/线程诊断。每个目录的中文说明区分历史真实验证、已知失败、公开路径重排和未完成边界。新目录不被 Cargo、安装脚本或 LaunchAgent 自动执行。

不包含 Apple sysroot、共享缓存、Xcode 包、二进制产物、个人进程日志或账号信息。所有性能数字都是相应历史版本与批次的局部证据，不能拼接为累计收益，也不能宣称达到原生 x86 95%。

另含 [独立上游 packed NZCV 组件](packed-nzcv-upstream/README.md)、[CPU 独立验证 fixture](cpu-fixtures/README.md) 和 [后续独立 MacFlow 可选源码补丁](macflow-latest/README.md)。独立 NZCV 的 AOT16 分支只用于 clean upstream，不能在主 AOT22 树重复套用。
