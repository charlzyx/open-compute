# open-compute 文档与参考

本目录保存持续维护的接口、测试、发布和运维资料。活动方案与历史实现从[文档索引](../README.md)进入。

## 文档位置

| 位置                | 内容                                 |
| ------------------- | ------------------------------------ |
| `docs/*.md`         | 仍需实施的方案                       |
| `docs/workerd/`     | 仍需实施的原生 workerd 方案          |
| `docs/implemented/` | 已完成需求的精简结果和当时证据       |
| `docs/releases/`    | 已发布版本的用户可见 release notes   |
| `docs/blocked/`     | 被外部条件阻塞且当前无法继续的方案   |
| `docs/references/`  | 持续维护的接口、测试、发布和运维资料 |

普通单篇文档不另建专用目录；同一规则只在所属入口维护，其余位置链接引用。

## 状态与移动

`planned` 表示仍需实施；`implemented` 表示代码已落地；`verified` 只表示列出的检查成功退出；
`accepted limitation` 需说明已接受的限制和影响；`blocked` 需说明外部阻塞、证据和恢复条件。

未完成方案留在 `docs/` 或 `docs/workerd/`；完成后删去调研、候选方案、实施顺序、完整 schema/API、逐文件任务、
测试矩阵和完成的 TODO，将结果移入 `implemented/`。接受的限制写入所属实现或参考文档；待修缺陷留在活动方案；
仅外部条件使工作无法继续时移入 `blocked/`。不单建 qualification、acceptance、Gate 或测试计划文档。
移动时同步索引、生成器和所有入链，不留 redirect、stub 或旧副本。

## 编号

需求、实施和阻塞文档必须沿用同一个编号，文件名使用小写编号前缀，不允许无编号文件：

| 前缀 | 范围                   | 示例                                                        |
| ---- | ---------------------- | ----------------------------------------------------------- |
| `P`  | open-compute 产品阶段  | `p20-cf-cli-migration.md`                          |
| `W`  | workerd 子项目阶段     | `w2-standard-limits.md`                                     |
| `I`  | GitHub issue 实施批次  | `i1-github-issues-1-3.md`、`i2-github-issue-4-r2-upload.md` |
| `G`  | 一次性调查或 Gate 研究 | `g1-test-repetition.md`                                     |
| `Q`  | 质量专项               | `q0-code-quality-2026-09-08.md`                             |
| `R`  | 跨既有阶段的架构重构   | `r0-localhost-worker-origins.md`                            |

子阶段和插入主线之间的补充阶段继续使用所属序列，例如 `P2.6` 写作 `p2-6-*`。同一需求从活动方案移动到
`implemented/` 或 `blocked/` 时编号不变。只有各目录 `README.md`、持续维护的
`references/` 与 `runbooks/`、以及以 SemVer 命名的 `releases/` 不使用上述编号。

`R` 只用于替换已经横跨多个产品阶段的 authoritative 架构，不用于普通功能、代码整理或质量修复；后者继续归入所属 `P`、`W`、
`I` 或 `Q`。重构完成后与其他活动方案一样以原编号移入 `implemented/`，不另建旧架构兼容文档。

## 内容与证据

- 先写用户结果、维护边界和实际验证；ownership、失败语义和非目标只写影响后续修改的部分。
- 原始输出与失败 artifact 留在 `.temp/` 或 CI；长期文档记录日期、输入、结论和证据标识。清理遵循[测试证据生命周期](testing.md#日志与保留证据)，`docs/implemented/g0-results.md` 原样保留。
- 历史 PASS 仅证明当时输入，不覆盖当前源码、机器可读合同和维护参考。旧客户端 trace、报告和已发布 release notes 保留原始身份，不能把旧 PASS 改写为 cf PASS。
- 当前应用入口遵循 [P20](../implemented/p20-cf-cli-migration.md)；活动方案同步命令、配置和验收 producer。Python 构建例外及移除条件由 [P25](../p25-platform-follow-ups.md) 管理。
- 文档改动运行 `git diff --check`、`bun scripts/check-docs.ts`，并核对所改内部链接、命令、状态和验证声明；该脚本检查网站文档，不覆盖本目录所有相对链接。新增/移动文档检查编号；根 `AGENTS.md` 和 `docs/references/**` 属于源码摘要输入，按[测试说明](testing.md#按输入选择测试)同步身份。

## 指令维护

按 [OpenAI 官方指南](https://developers.openai.com/api/docs/guides/latest-model#prompting-best-practices)核对自主执行、指令冲突与验证范围；以下是本仓库的维护约定，不是模型版本或 API 迁移要求。

- 根 `AGENTS.md` 保留全局约束、授权边界和任务入口；目录规则只补局部要求，参考文档拥有操作细节。按当前任务读取相关资料，不要求每次遍历全部文档。
- 规则写清适用条件、目标和完成标准。合并重复条款，删除已过时的流程；保留安全、数据完整性、正式 pin、发布迁移和验收边界。历史证据与现行指令分开。
- 授权和冲突处理引用根 [Operating Contract](../../AGENTS.md#operating-contract)，不另立审批流程，也不把历史计划中的步骤当作当前任务授权。
- 修改后核对规则是否仍可执行、是否误扩验证范围；不以文档缩短推断质量、成本或模型表现已经改善。

## 维护资料

| 文档                                                  | 用途                                                                     |
| ----------------------------------------------------- | ------------------------------------------------------------------------ |
| [Host authority](host-authority.md)                   | 本机与公网 ingress 共用的 hostname ownership、解析和 endpoint projection |
| [Cloudflare 兼容矩阵](cloudflare-compatibility.md)    | 当前支持面和 deviation                                                   |
| [Cloudflare 上游刷新](cloudflare-upstream-refresh.md) | OpenAPI、官方 SDK 与 cf 的定期发现和协调升级                             |
| [能力偏差](p1-deviations.md)                          | 当前 deviation ID 与边界                                                 |
| [Containers 运维](containers.md)                    | 独立 Broker、provider 资格与 execution 恢复边界                           |
| [测试节奏](testing.md)                                | Gate、case discovery、覆盖率和验收                                       |
| [单二进制分发](single-binary.md)                      | 构建输入、离线启动和发行合同                                             |
| [发布流程](releasing.md)                              | 版本、tag、release workflow 和校验                                       |
| [CI 构建性能](ci-build-performance.md)                | 当前 CI、并行和缓存取舍                                                  |
| [open-compute workerd fork](workerd.md)               | fork 身份、定制能力、正式 pin 与独立二进制下载                           |
| [workerd 上游](workerd-upstream.md)                   | 上游能力与 fork 升级重点                                                 |
| [Vinext 输入](vinext-input-validation.md)             | 固定输入与离线校验                                                       |
| [Fuzz 所有权](p1-fuzz-ownership.md)                   | 输入 corpus 与回归归属                                                   |

运维手册位于 [`runbooks/`](runbooks/)，由 `ocd` 编译时内嵌。调整路径或名称时必须同步资源读取和测试。
