# 文档索引

| 内容                     | 权威入口                                                                                    |
| ------------------------ | ------------------------------------------------------------------------------------------- |
| 已实现架构与产品维护     | [完成索引](implemented/README.md)                                                           |
| 正式版本说明             | [Release notes](releases/README.md)                                                         |
| 当前 API 支持与偏差      | [兼容矩阵](references/cloudflare-compatibility.md)、[偏差清单](references/p1-deviations.md) |
| 开发测试、部署与运维     | [参考文档](references/README.md)                                                            |
| 原生运行时实施与后续工作 | [workerd 路线](workerd/README.md)；源码基于 `third_party/workerd/` submodule                |
| 其他待实现设计           | 下表；外部前置阻塞见 [blocked](blocked/README.md)                                           |

按任务查阅对应入口；文档位置、状态、证据与指令维护统一遵循[文档规则](references/README.md)。
当前应用入口见 [P20](implemented/p20-cf-cli-migration.md)；历史 Wrangler 记录不构成当前 cf 兼容承诺。
[P23 Containers 基础实现](implemented/p23-cloudflare-containers.md) 记录基础 13 family 与本轮验证。
最终 workspace Gate 为 62/63、失败；P21 原目标 scoped 3/3 PASS 不改写该结果。
Linux ARM 与 Linux x64 已完成本次 package 范围。
Darwin Q10 原四项 package 用例 4/4 PASS；测试进程已回收。
两组 ACL 已恢复；临时账号、home 和 staging 已删除，账号进程为零。
完整 64 个稳定成员仍保持 `blocked`，剩余能力由 P25 跟进。

## 待实施

| 文档                                                                               | 当前状态                                                                                                                                                                                                                                                                                |
| ---------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [W4 workerd fork 收薄](workerd/w4-thin-fork.md)                                    | planned：逐组复核整个 fork；保留必要执行器与宿主接口，优先替换 Service／DO 私有 RPC 和 Queue 重复路径；替代能力尚待验证                                                                                                                                                                 |
| [P24 macOS Developer ID 签名与 Apple 公证](p24-macos-code-signing-notarization.md) | Day 1 发行合同与 CI 方案完成；待配置受保护的 Apple/GitHub 凭据、签署最终 `ocd`、取得 Notary `Accepted` 并完成真实 tag 验收                                                                                                                                                              |
| [P25 平台后续能力](p25-platform-follow-ups.md)                                     | planned：instance 显式 `.env`、operator logger、lazy Worker startup、临时 Python 构建桥接移除及 Flask SDK 流式问题跟进；Container 剩余稳定能力与 differential                                                                                                                                                                  |
| [P26 外部反向代理下的 Worker 自定义域名](p26-external-proxy-worker-origins.md) | planned：复用 hostname authority 与 public-origin，增加 tenant-only external ingress、instance exact-host 授权、显式对外 origin 和独立 endpoint projection；关联 #148 |

[P20 已实现 CLI/应用构建入口](implemented/p20-cf-cli-migration.md) 是应用入口的权威方案；[P21 Python Workers](implemented/p21-python-workers.md) 记录普通 Python 部署、prepared artifact 与真实 daemon 验收。Python 构建临时调用用户安装的 PyWrangler，不安装或校验版本；上传、部署、认证和资源管理保持 cf。桥接移除与已知 Flask SDK 流式问题继续由 [P25 活动待办](p25-platform-follow-ups.md) 跟进。
