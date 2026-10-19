# open-compute workerd fork

open-compute **不使用 Cloudflare 官方发布的 stock workerd 二进制**。生产运行时来自
[`elliothux/workerd`](https://github.com/elliothux/workerd)，它是
[`cloudflare/workerd`](https://github.com/cloudflare/workerd) 的定制 fork。`ocd` 会把与当前目标匹配的固定
workerd archive 内嵌进自己的单二进制发行物；启动时离线校验并物化，不从 `PATH` 查找，也不会下载或切换到 stock workerd。

当前 formal pin 的唯一 authority 是
[`packages/runtime/workerd.lock.json`](../../packages/runtime/workerd.lock.json)：

| 身份                       | 当前值                                                                                             |
| -------------------------- | -------------------------------------------------------------------------------------------------- |
| fork release               | `v1.20260930.0-open-compute-r21.204a75bcf`                                                         |
| fork revision              | `204a75bcfcc35fdf15756bef03eea867711847a6`                                                         |
| upstream base              | `d99bc6b777e35d72d71c2f1fe2fd1db53284528a`（正式 pin 的 upstream base）                            |
| `--version`                | `workerd 2026-09-30`                                                                               |
| private Container protocol | `open-compute-container-protocol-v3`                                                               |
| build workflow             | [Open Compute binaries 39042535973](https://github.com/elliothux/workerd/actions/runs/39042535973) |

2026-10-17（UTC），R21 的四个平台 build、原生 Container 资格和 publish 均成功。
每个平台通过 7 个原生 owning target 和 11 项 Root 回归。
不可变 Release 的 10 项资产已通过签名、GitHub digest 和 `SHA256SUMS` 校验。
archive 使用 GNU `gzip -9n`，mtime 为 0 且无文件名。
Release 包含逐目标 build-inputs、catalog 与 `SHA256SUMS`。
下表摘要来自已验证的资产与正式 lock。

R21 保持 private Container protocol v3。
R21 在 `output()` 等待输出和退出码时持有 process handle。
输出流排空后，该 handle 可以释放。
R20 让 snapshot capture 在异步 lease 查询后使用既有 Root control event。
R21 保留 weak owner、exact lease、公开 binding 回调与 Socket 所有权边界。
原生资格不代表完整产品验收。
R21 的真实产品、full65、coverage、Final 与三主机 package 验收仍待完成。

历史 R16 的两次 Darwin x64 资格均超时；具体原因仍未确认。
R17 的 v3 协议已包含 named images、snapshot host callback、exec 子进程证明与 inactivity 修复。
R17 将 Rosetta 跨架构 medium 测试预算从 30 秒设为 60 秒。
R21 保留这个有界预算，未增加产品超时。
R18 的 Linux ARM64 资格在一条测试的 NUL 终止符断言失败。
R19 相对 R18 仅把该断言改为精确字节数组比较。
历史记录见 [workerd 方案](../workerd/README.md#r19-发布与输入校验2026-10-13)。
R9、R15、R17、R18、R19、R20 与更早结果只适用于各自输入，不能替代 R21 产品验收。

`workerd --version` 只显示上游日期，不能证明拿到的是本 fork。需要同时核对 revision、目标和 lock 中的 binary
SHA-256。

## Fork 扩展

fork 保留 upstream Worker runtime、module validation、RPC、Durable Objects 与 Loader 基础实现，只在 standalone
宿主缺少的边界增加下列能力：

| 扩展                       | fork 提供的能力                                                                                                                                                                   | open-compute 中的用途                                                                                                      |
| -------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| Dynamic Worker Loader      | delegated Loader namespace、受约束 capability delegation、原生 `load` / `get`、entrypoint/RPC、Dynamic Durable Object facets、tails、in-flight accounting 与撤销生命周期          | 向普通 Worker 提供正式的 Worker Loader binding，同时保持 account、Script、Version 与 binding namespace 隔离                |
| Workers Standard limits    | standalone CPU、memory、startup CPU、subrequest 与 simultaneous outbound-connection enforcement；Dynamic Worker/entrypoint/delegated Loader ceiling 组合；超限 isolate 摘除与恢复 | 执行 cf/v4 Settings 与 immutable Version 中的 limits，而不是只解析参数或依赖 Cloudflare 私有宿主                           |
| Native host extensions     | 私有 `HostExtensionFactory` / `HostExtensionPort`、generation broker fd 4、session-scoped Cap'n Proto unary/stream transport                                                      | 让 operator-owned native Provider 通过普通 Service Binding facade 服务 Worker；该 ABI 不是 Cloudflare 标准 API             |
| Dynamic binding forwarding | 私有 `openComputePrivateEnv`、host-issued Loader grant、handler-only capability table 与 generation/revocation fence                                                              | 由 `open-compute:worker-loader` 显式转发 KV、D1、R2、Queue 和普通值，同时不把根 binding 变成可 structured-clone 的公共对象 |
| Dynamic Container attachment | 原生 Container facet、owner 内的 outbound callback capability、活动跟踪与递归撤销 | 仅向声明的 DO class 附加原生 Container；持久租约、资源和 Engine 终止由平台 authority 管理 |
| Compatibility catalog      | `compatibility-catalog` 从编入 binary 的 maximum date 与 `CompatibilityFlags` schema annotations 生成确定性 JSON；不加载配置或启动 listener                                       | 让 build、CLI、management API、SDK 与 Dashboard 从 exact binary 发现同一 date/flag 合同                                    |
| Reproducible binaries      | 四目标优化构建 workflow、固定编译器/Bazel/config 与 canonical release archive 生成输入                                                                                            | 为 formal pin、GitHub Release build input 和跨平台产品 Gate 提供可复现来源                                                 |

这些扩展不代表 Cloudflare 托管平台采用相同内部实现。公开 Cloudflare-compatible surface 与 open-compute
私有扩展仍分别记录；实验性 Loader 控制、完整 Workers for Platforms 和 dispatch namespace 不会因为使用 fork 而自动获得支持。
实现与边界详见 [W1 Worker Loader](../implemented/w1-dynamic-workers-worker-loader.md)、
[W2 Standard limits](../implemented/w2-standard-limits.md)、
[W3 native bindings](../implemented/w3-user-extensible-native-bindings.md) 和
[I102 binding forwarding](../implemented/i102-dynamic-worker-binding-forwarding.md)。

## 单独下载 workerd

以下 archive 来自 fork 的不可变 GitHub Release，而不是 Cloudflare release。下载后应同时核对 archive 与解压 binary
SHA-256；唯一 authority 仍是 formal lock。

二进制沿用 workerd 的 Apache 2.0 license，并包含 upstream source tree 记录的第三方组件；它们由 open-compute
项目发布和支持，不是 Cloudflare 官方发行物。

| Target           | Archive                                                                                                                                            | Archive SHA-256                                                    | Binary SHA-256                                                     |
| ---------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| macOS ARM64      | [workerd-darwin-arm64.gz](https://github.com/elliothux/workerd/releases/download/v1.20260930.0-open-compute-r21.204a75bcf/workerd-darwin-arm64.gz) | `e73e9de735d6b81123d1b43394161215252fde9bd0df637642936a330c2708c0` | `ae2170b92d9aef34d3f52c83d0f3f6c8c70e819dcbd9a30eb9bcdff0124250e0` |
| Linux GNU ARM64  | [workerd-linux-arm64.gz](https://github.com/elliothux/workerd/releases/download/v1.20260930.0-open-compute-r21.204a75bcf/workerd-linux-arm64.gz)   | `4c2d06af045a15ff591766b9e60a215cadc615d77f223cddb58c0cb324bb2321` | `c8423885ec2679f2b82bcee7c32ee80d76355e075b908e76b4fb95238476ae76` |
| Linux GNU x86-64 | [workerd-linux-64.gz](https://github.com/elliothux/workerd/releases/download/v1.20260930.0-open-compute-r21.204a75bcf/workerd-linux-64.gz)         | `2a642f6846bc9d36c8bf56466a574e01e9f0de9a4458de84b421304b0f12c111` | `09ed637dd36b47ea10d601a1aec2118a26fb573f20ecb9751734466ae0fc6f7f` |
| macOS x86-64     | [workerd-darwin-64.gz](https://github.com/elliothux/workerd/releases/download/v1.20260930.0-open-compute-r21.204a75bcf/workerd-darwin-64.gz)       | `ed42350f1fc0859ef0528899282219ae41a4a4e3f823fe2ac39c9fe807be9b72` | `5ef9cd16ffe22ed99dc410d9cd605060430901feeec7b41a7c37ecdee8f66bd8` |

例如下载 Linux x86-64 版本：

```sh
curl -fL https://github.com/elliothux/workerd/releases/download/v1.20260930.0-open-compute-r21.204a75bcf/workerd-linux-64.gz -o workerd.gz
echo '2a642f6846bc9d36c8bf56466a574e01e9f0de9a4458de84b421304b0f12c111  workerd.gz' | sha256sum -c -
gzip -dc workerd.gz > workerd
echo '09ed637dd36b47ea10d601a1aec2118a26fb573f20ecb9751734466ae0fc6f7f  workerd' | sha256sum -c -
chmod +x workerd
./workerd --version
```

macOS 可把校验命令替换为 `shasum -a 256 workerd`。单独 binary 适合源码/配置验证和 fork 调试；它不包含 `ocd`
拥有的实例注册、SQLite authority、deployment compilation、secret、Gateway、supervision 与恢复逻辑，不能替代完整
open-compute 安装。

## 升级与源码

fork 源码由 [`third_party/workerd/`](../../third_party/workerd/) submodule 固定。每次升级应一起更新 fork revision、
upstream base、四目标 release asset、archive/binary digest、binary maximum、compatibility catalog schema/digest、system Worker
date/flags 和真实 runtime Gate；不能只替换其中一个文件。四目标 workflow 应执行 introspection 两次并比较全部
catalog 字节；根 build 再对 host binary 重算 catalog，任何 revision/binary/maximum/catalog drift 都在 Cargo 前失败。上游能力与
fork-ahead 审查见 [workerd upstream](workerd-upstream.md)，构建和内嵌合同见
[单二进制分发](single-binary.md)。
