# 独立窗口和线程呈现诊断

三项仅为有限诊断源码/runner，不修改桥、不安装组件：

| 目录 | 历史实际结果 |
| --- | --- |
| `window-baseline` | IOSurface→CALayer native/ARM 三帧 GPU 绘制、196608 像素回读及 layer.contents 附着通过；原默认桥的 ARM CAMetalLayer/MTKView 在 commandBuffer presentDrawable 编码时 SIGBUS，失败明确保留。 |
| `local-main` | GPU fence 后由 guest 主线程直接 `drawable.present`；native/ARM × 两种 view 共四进程各三次 GPU 完成/present 返回/真实 callback 通过。有零 presentedTime，未验证屏幕图像。 |
| `thread-warmup` | 显示层安装后独立 0.2 秒预热、连续 12 帧；native/ARM × 两种 view 48 次 GPU 完成/worker present 返回/callback，36 非零与 12 零 presentedTime，24 非零在连续阶段。 |

原 v3 没有安装后预热，48 个 presentedTime 全为 0；v3.1 原 runner 用半开 span 的 `<` 误判相邻日志失败。这里保留最终 `<=` 解析器以及九项纯字符串自测；本地对旧日志做的是数据重新核验，没有重跑旧 probe。线程结果只能说 probe 没有显式主 runloop 泵；框架可能处理主线程事件，真实日志有一次主线程 callback。

这些证据均不证明逐帧屏幕颜色、物理 drawable 池复用、一般线程安全、零拷贝或速度。公开 runner 把原本机 SHA 约束改为所提供输入的前后身份记录，不改变 probe。每进程 30 秒期限，只清理自己的进程组。

```sh
python3 thread-warmup/check_parser_data.py
python3 thread-warmup/run_presentation_probe_v31.py --runtime /absolute/maclator --sysroot /absolute/arm64-sysroot --gpu-dir /absolute/gpu --output /absolute/new-results --modes all
```

另两 runner 使用相同的 runtime/sysroot/gpu-dir/output 参数，运行前请先查看各自 `--help`。
