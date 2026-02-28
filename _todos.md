# matrix-bridge-feishu 完成度提升任务清单（Feishu 定向）

更新时间：2026-02-28  
依据：`_diff.md` + 飞书机器人/IM 官方文档（见文末链接）

目标：
- 把当前“核心链路可用”提升到“协议对齐、行为完整、可运维、可回归”的工程状态。
- 明确只做飞书能力适配，不盲目追求与 Discord 项目 1:1 功能等量。

## P0（必须先完成，形成可用闭环）

1. [x] T001：补齐 Feishu 消息 API 客户端（create/reply/update/delete/get/resource）
实施要点：在 `FeishuClient` 增加统一请求层、错误码映射、结构化响应；新增 `send_message`（可选 `receive_id_type` + `uuid`）、`reply_message`、`update_message`、`recall_message`、`get_message`、`get_message_resource`。
验收标准：Matrix 普通消息、回复、编辑、撤回分别命中飞书对应 API；失败时日志能输出接口名、code、msg、message_id。
涉及文件：`src/feishu/client.rs` `src/feishu/service.rs` `src/feishu/types.rs`

2. [x] T002：重构 Matrix->Feishu 发送路径，按事件语义调用不同接口
实施要点：
- 普通消息 -> `message/create`
- 回复消息 -> `message/reply`（支持 `reply_in_thread`）
- 编辑消息（`m.replace`）-> `message/update`
- 红线删除（`m.room.redaction`）-> `message/delete`
- 为每次 create/reply 生成 `uuid` 去重键（1 小时窗口）
验收标准：Matrix 侧四类事件行为与飞书侧展示一致，不再全部降级为 `send_text_message`。
涉及文件：`src/bridge/event_processor.rs` `src/bridge/message_flow.rs` `src/bridge/feishu_bridge.rs`

3. [x] T003：补齐 Feishu->Matrix 事件处理（接收消息 + 撤回消息）
实施要点：
- 支持 `im.message.receive_v1` 与 `im.message.recalled_v1`
- 收到 recalled 事件时，通过 mapping 找到 Matrix event 并发 redaction
- 接收消息时按 `msg_type` 解析，而非统一占位文本
验收标准：飞书撤回一条已桥接消息后，Matrix 对应消息被撤回；日志可追踪 `message_id/chat_id`。
涉及文件：`src/feishu/service.rs` `src/bridge/feishu_bridge.rs` `src/database/*`

4. [x] T004：实现飞书消息内容结构的完整解析与构造
实施要点：覆盖 `text/post/image/file/audio/media/sticker/interactive` 的收发 JSON；保留 mention 信息与附件 key；把当前 formatter 的占位实现改成结构化转换。
验收标准：常见类型不再输出 `[Image]/[File]/[Card]` 占位；至少文本、富文本、图片、文件四类可双向闭环。
涉及文件：`src/formatter/feishu_to_matrix.rs` `src/formatter/matrix_to_feishu.rs` `src/bridge/message_flow.rs`

5. [x] T005：补齐媒体链路（上传 + 下载 + 尺寸限制）
实施要点：
- Matrix 媒体 -> 下载 mxc -> 调用 `im/v1/image` 或 `im/v1/files` 上传 -> 飞书发消息引用 `image_key/file_key`
- Feishu 媒体 -> `message.resource/get` 拉取二进制 -> 上传到 Matrix
- 按官方限制加校验（图 10MB、文件 30MB、资源下载 100MB）
验收标准：图片/文件可以双向真实传输；超限时返回明确错误，不 panic。
涉及文件：`src/feishu/client.rs` `src/bridge/message_flow.rs` `src/bridge/feishu_bridge.rs`

6. [x] T006：严格对齐事件订阅安全与 URL 校验
实施要点：
- 保留并强化签名校验：`sha256(timestamp + nonce + encrypt_key + raw_body)`
- 兼容 Verification Token 校验
- `url_verification` 在 1 秒内返回 challenge
- 统一处理加密/明文请求体
验收标准：
- 开发者后台可成功保存回调地址
- 错误签名请求返回 401
- 有加密策略时可正确解密事件
涉及文件：`src/feishu/service.rs` `src/feishu/client.rs`

7. [x] T007：事件幂等与防重放治理
实施要点：
- 入站去重以 `message_id` 为主，不依赖 `event_id`
- 出站请求使用 `uuid`
- 为重放请求记录可观测指标
验收标准：重复推送同一飞书消息不会重复入 Matrix；重复发同 uuid 不会重复发送。
涉及文件：`src/database/models.rs` `src/database/sqlite_stores.rs` `src/bridge/feishu_bridge.rs`

8. [x] T008：引入异步处理队列，Webhook 快速 ACK
实施要点：接收 webhook 后快速确认，消息桥接异步执行；增加 per-chat 顺序队列，避免乱序与阻塞。
验收标准：在高并发 webhook 下无明显超时；同一 chat 内消息顺序稳定。
涉及文件：`src/feishu/service.rs` `src/bridge/*`

## P1（增强稳定性和可运维性）

9. [ ] T009：补齐群生命周期事件适配（成员进出群、群配置更新、群解散）
实施要点：处理 `im.chat.member.user.added_v1`、`im.chat.member.user.deleted_v1`、`im.chat.updated_v1`、`im.chat.disbanded_v1`；同步房间映射状态与 bridge metadata。
验收标准：飞书侧群解散后，映射自动失效；群模式切换到 thread 时桥接策略自动更新。
涉及文件：`src/feishu/service.rs` `src/bridge/portal.rs` `src/database/*`

10. [ ] T010：落地 thread/topic 语义桥接
实施要点：维护 `thread_id/root_id/parent_id` 映射；支持 Matrix 回复映射到飞书话题回复；支持从飞书 thread 回复回到 Matrix reply 关系。
验收标准：双向回复链路可追踪，不再丢失上下文。
涉及文件：`src/bridge/message_flow.rs` `src/database/models.rs` `src/feishu/types.rs`

11. [ ] T011：数据库层补齐 PostgreSQL stores（或显式降级声明）
实施要点：
- 方案 A：实现 Postgres 对应 stores，移除 “stores 仅 sqlite” 限制
- 方案 B：若短期不做，配置层禁止 postgres 并文档明确
验收标准：配置与实际能力一致，不出现“可配不可用”。
涉及文件：`src/database/mod.rs` `src/database/sqlite_stores.rs` `src/bridge/feishu_bridge.rs` `README*`

12. [ ] T012：错误分类与重试退避
实施要点：按飞书 code 分类（鉴权失败、权限不足、频率限制、参数错误）；对可重试错误增加指数退避。
验收标准：重试策略可配置，日志区分不可重试错误与可重试错误。
涉及文件：`src/feishu/client.rs` `src/config/*` `src/util/*`

13. [ ] T013：可观测性增强（metrics + tracing 字段标准化）
实施要点：新增 Prometheus 指标（入站事件量、出站调用量、失败码、处理耗时、队列深度）；日志统一携带 `matrix_event_id/feishu_message_id/chat_id`。
验收标准：`/metrics` 可抓取；可基于日志完整追一条消息链路。
涉及文件：`src/web/*` `src/bridge/*` `src/feishu/*`

14. [ ] T014：配置系统增强（环境变量覆盖 + 严格校验）
实施要点：支持 `CONFIG_PATH`、关键字段 env override；校验占位符配置值；校验权限/开关组合合法性。
验收标准：错误配置在启动时快速失败并给出可操作报错。
涉及文件：`src/config/mod.rs` `src/config/bridge.rs` `example-config.yaml`

15. [ ] T015：Provisioning API 安全化
实施要点：为 `/admin`、`/_matrix/app/v1` provisioning 路由增加认证与审计日志；限制高风险操作（删除映射）权限。
验收标准：未授权请求无法进行桥接管理操作；审计日志包含操作者与变更对象。
涉及文件：`src/web/provisioning.rs` `src/bridge/provisioning.rs`

## P2（完善测试、文档与交付）

16. [ ] T016：单元测试补齐（解析、格式转换、安全校验）
实施要点：
- webhook 签名/解密测试
- 各 `msg_type` 解析与构造测试
- `m.relates_to` -> reply/edit 转换测试
验收标准：新增关键模块测试覆盖，避免回归破坏核心桥接逻辑。
涉及文件：`src/feishu/client.rs` `src/feishu/service.rs` `src/formatter/*` `src/bridge/message_flow.rs`

17. [ ] T017：集成测试（Mock Feishu + Mock Matrix）
实施要点：构建本地 mock server，回放 `receive_v1/recalled_v1` 和 message API 响应；校验数据库 mapping 与最终发送行为。
验收标准：CI 可自动跑通消息收发、编辑、撤回、媒体最小闭环。
涉及文件：`tests/*` `.github/workflows/tests.yml`

18. [ ] T018：文档与样例配置升级
实施要点：
- README 增加“已支持/未支持”能力矩阵（按 Feishu msg_type/event）
- 补充必需权限清单与后台配置步骤（事件订阅、加密策略、机器人权限）
- 给出排障手册（签名失败、权限不足、频率限制）
验收标准：新同学按 README 能在 30 分钟内完成最小可用部署。
涉及文件：`README.md` `README.zh-CN.md` `example-config.yaml`

19. [ ] T019：发布前验收脚本
实施要点：新增 `scripts/release-check.ps1`（或等价脚本）自动检查：配置字段、数据库连接、回调可达性、API 权限、关键端点健康状态。
验收标准：发布前可一键跑自检，失败点可定位到具体检查项。
涉及文件：`scripts/*` `README*`

## 建议执行顺序（里程碑）

- M1（可双向稳定收发）：T001-T008
- M2（协议语义基本完整）：T009-T015
- M3（可回归可发布）：T016-T019

## 文档依据（Feishu）

- Bot 概述：<https://open.feishu.cn/document/client-docs/bot-v3/bot-overview>
- 接收消息事件：<https://open.feishu.cn/document/server-docs/im-v1/message/events/receive>
- 消息 API：
  - 发送：<https://open.feishu.cn/document/server-docs/im-v1/message/create>
  - 回复：<https://open.feishu.cn/document/server-docs/im-v1/message/reply>
  - 编辑：<https://open.feishu.cn/document/server-docs/im-v1/message/update>
  - 撤回：<https://open.feishu.cn/document/server-docs/im-v1/message/delete>
  - 获取消息：<https://open.feishu.cn/document/server-docs/im-v1/message/get>
  - 获取消息资源：<https://open.feishu.cn/document/server-docs/im-v1/message/get-2>
- 媒体上传：
  - 图片：<https://open.feishu.cn/document/server-docs/im-v1/image/create>
  - 文件：<https://open.feishu.cn/document/server-docs/im-v1/file/create>
- 事件/回调安全与解密：
  - 接收回调：<https://open.feishu.cn/document/event-subscription-guide/callback-subscription/receive-and-handle-callbacks>
  - 将回调发送至开发者服务器：<https://open.feishu.cn/document/event-subscription-guide/callback-subscription/step-1-choose-a-subscription-mode/send-callbacks-to-developers-server>
- 话题能力：<https://open.feishu.cn/document/im-v1/message/thread-introduction>
- 自建应用 tenant_access_token：<https://open.feishu.cn/document/server-docs/authentication-management/access-token/tenant_access_token_internal>
