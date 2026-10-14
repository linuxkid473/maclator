# 原生内存同步故障：独立 v2 原型

这一包没有注册到主运行时，也不默认安装信号处理器。`native-fault/` 为 C/REP assembly 叶函数和严格的 DFL-only installer，`native_fault.rs` 为 Rust FFI 所有权适配器。host v2.1、Rust adapter、early-owner 是三组不同范围的测试；不能据此宣称 guest 信号恢复已完成。

| 历史测试 | 结果与范围 |
| --- | --- |
| host v2.1 | 41/41；受保护页实际读/写 fault 的精确 REP 进度、DF/errno，以及独立 white-box 拒绝和 installer 失败注入。 |
| Rust adapter | direct/observed 两模式共 26/26；ABI 字段、正常返回、原子释放、六个受保护页 fault；手动完成剩余主机区间。 |
| early-owner | 九个私有 C constructor/Rust startup 实验通过；未知现存 handler/IGN 零写拒绝、后续 owner 变化。仅对审查过的 Rust 1.99.0 Darwin 工具链成立。 |

原 v1 隐藏 one-shot handler 恢复失败、首次 v2 编译问题与 Rust 首次路径问题在本地历史记录中保留，未被成功测试覆盖或改称通过；公开包只保留修订源码、测试与这份说明，不上传本机原始日志。

尚未完成：生产 handler 所有权协议、真正 guest FAR/ESR/frame/token、shim RESUME、guest sigreturn、非局部退出清理、完整异步/fork/线程并发与 registry exhaustion、真实栈溢出诊断。不认识的既存 handler 不应由生产程序重置为 DFL；测试中显式重置只在独立测试进程执行。没有翻译吞吐新成绩。

公开版只把绝对 include/build 路径改为相对路径，并将依赖私有历史日志的 guard 改为 `MANIFEST.json` 本包身份检查。它有自己的源 SHA；历史运行结论不能自动当作重排版的新实测。以下脚本不自动安装、不需要 sudo，每个测试进程有 20 秒期限：

```sh
bash host/build-host.sh
python3 host/run-bounded.py --suite all --timeout 20
bash rust-adapter/build-host.sh
python3 rust-adapter/run-bounded.py --suite all --timeout 20
bash early-owner/build-host.sh
python3 early-owner/run-bounded.py --timeout 20
```

测试需真实 x86_64 macOS、clang 与 Rust。每轮写入新的结果目录，只清理自己创建的进程组。早期 owner 构建会检查编译器版本及 LC_MAIN/initializer metadata；这些静态信息仍不能单独证明所有库的初始化顺序。

本次公开路径重排后三组 build-host.sh 已在 x86_64 macOS 上实际编译通过：三个 C host 产品、两个 Rust adapter 产品、九个 early-owner 产品；-Wall/-Wextra/-Werror 与 -D warnings 保留。这里没有执行任何新 fault/handler 测试，历史运行结论仍单独标注。构建生成目录不参与公开源码提交。
