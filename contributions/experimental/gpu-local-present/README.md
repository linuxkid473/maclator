# 普通 presentDrawable：默认关闭的桥候选

`gpu-local-present.patch` 为最终 revision2-repro 修改，基线是本 PR 根目录 `gpu/mclmetal.m`；补丁开关 `MCL_LOCAL_PRESENT=1` 默认关闭。公开构建只复制源到新输出目录，不覆盖根目录或已安装桥。

它在参数编码前识别真实 drawable，记录呈现，等待实际 host GPU commit/完成，再由 guest 主线程同步 present；修订修复了 objectOut 泄漏和内部 status 查询异常。历史 native 2 + ARM default/legacy/unbatched 六个有限进程（CAMetalLayer/MTKView）均通过，ARM 原编码 SIGBUS 未重现；每进程三次 GPU 完成、真实呈现 callback。桥模式下 Metal 反射、绘制/计算回读和 NSError 检查通过。

每进程首 callback 的 presentedTime 仍为 0；探针没有屏幕像素验证。主线程不泵消息时 drawable 池可能耗尽；timed present、worker 提交、resize、多 layer、长跑引用与实际失败注入未验证，legacy drop 原有 no-op 未解决。它没有部署为当前默认桥，也没有性能提升成绩。

```sh
bash build.sh /absolute/new-candidate-dir
python3 run_candidate_probe.py --runtime /absolute/maclator --sysroot /absolute/arm64-sysroot --gpu-dir /absolute/new-candidate-dir --output /absolute/new-results --modes all --arm-mode all
```

runner 只运行用户明确提供的 runtime/bridge，在开始/结束记录真实 SHA，不再要求本机历史 binary SHA。公开重排后的脚本与历史日志是两份身份；历史结果不能当作这份新包已经运行。
