# 后续 MacFlow 界面与性能反馈：可选源码补丁

本补丁贡献独立 MacFlow 后续工作，来源本地提交 67ba56f：原生五页 AppKit 界面、拆分 UI model/chart/history/widgets/icon 模块、真实历史读取边界、键盘/辅助功能名称、可选应用规则、维护/退出确认与性能反馈分类、恢复/隔离测试。它没有替换主运行时绑定的冻结协议。

显式基线为本 PR 的 integration/macflow；每个变更文件的前后 SHA 见 BASE-MANIFEST.json。在新的目录复制基线再应用，保留根工作区：

    cp -R /absolute/maclator/integration/macflow /absolute/new-MacFlow-workspace
    cd /absolute/new-MacFlow-workspace
    git apply --check /absolute/maclator/contributions/experimental/macflow-latest/latest-workspace.patch
    git apply /absolute/maclator/contributions/experimental/macflow-latest/latest-workspace.patch
    cargo test --workspace --locked
    cargo build --release --workspace --locked

对比 frozen protocol：RuntimeHello、RuntimePolicy、RuntimeStats、RuntimeMessage 字段保持相同；后续增加控制状态、PerformanceFeedback 和命令/status 默认值。字段对照不替代整合重建、真实 IPC 与回归；不应直接以新协议覆盖当前已验收 runtime 的依赖。主默认源码仍使用对应冻结协议。

历史后续工作区 release/单元检查为 106 passed、0 failed、1 ignored；原生界面布局/有限资源测量通过，完整键盘/VoiceOver、实际应用性能、24 小时压力与七天日常验收未完成。此前九组 p95 中位改善约 10.24%，低于目标 20%；Maclator 95% 原生性能目标仍独立未达。这里不上传真实用户进程导出、界面截图、本机配置、签名产物或运行日志。

公开移植只泛化本机路径与文档，保留基线已经参数化的 UID/GID 验收脚本。package.py 只生成新包；安装、root helper、lifecycle 与部署脚本不在 Cargo 构建时自动执行。若在独立 workspace 使用 deploy_maclator.py，需明确设置 MACLATOR_SOURCE_ROOT 为要部署的 Maclator 仓库；发布贡献本身没有运行任何这些系统操作。
