# 原生内存桥和早期循环实验

这三个历史分支未进入当前默认运行时。补丁与受到修改的精确基线文件一同保留，适合阅读差异和 `git apply --check`；这些局部基线不是完整可编译仓库，不应直接覆盖当前 AOT22 运行时。

| 分支 | 显式基线 | 开关与历史验证边界 |
| --- | --- | --- |
| `native-platform.patch` | `base-before-platform`，本地早期成熟版本 | `MACLATOR_NATIVE_PLATFORM=1` + ARM interpose shim；实际 memcpy/memmove/memset/bzero、tag/errno/重叠/子进程检查通过。同 binary 九轮总量约 1.0577×、memops 约 1.6028×。非法指针的同步故障没有接入 guest 恢复，默认关闭。 |
| `fixed-pair.patch` | `base-platform`，原生内存桥版本 | MACLATOR_JIT_PAIR_FIXED=1，AOT21 bit9；五组各 1916 有效循环、180 pair fixtures、1260 预算检查点，27 Rust tests 和真实 ARM pair-access 通过。无性能结论、未安装。 |
| `fixed-lhs.patch` | `base-platform`，与上一项并列 | `MACLATOR_JIT_FIXED_LHS=1`；六种预算/循环组合各 7184 完整循环通过，CSEL 16 条件检查通过，未安装。 |

两个 fixed 补丁是**并列实验**，不要依次套用。主运行时后来发展了不同的循环缓存与预算正确性实现，这里不宣称其补丁可以无冲突合入。补丁内的 `platform/README.md`、ARM shim 和内存回归源也已贡献，不包含系统库或 sysroot。

检查方法（在本目录）：

```sh
(cd base-before-platform && git apply --check ../native-platform.patch)
(cd base-platform && git apply --check ../fixed-pair.patch)
(cd base-platform && git apply --check ../fixed-lhs.patch)
```

基线及补丁 SHA 见 `MANIFEST.json`。性能只是历史相应 binary 的结果，不能与最新版本成绩相加，不能代表达到原生性能 95%。
