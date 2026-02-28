# Matrix Appservice Feishu（中文说明）

一个基于 Rust 与 Salvo Web 框架实现的 Matrix <-> 飞书桥接服务。

- English: `README.md`
- 中文说明: `README.zh-CN.md`

## 功能特性

- Matrix 与飞书**双向消息桥接**
- **富文本与卡片消息**转换支持
- **文件/媒体共享**（图片、视频、文档等）
- **用户与房间映射同步**
- 基于飞书 API 的 **Webhook 集成**
- Rust 实现，具备**高性能与稳定性**
- 基于 SQLite 的桥接映射与事件状态持久化
- 结构化日志与完整错误处理
- 支持飞书回调加密消息处理

## 架构

项目采用模块化架构：

- **Bridge Core**：核心桥接逻辑与状态管理
- **Feishu Integration**：飞书 API 客户端与 webhook 处理
- **Database Layer**：基于 Diesel 的持久化层
- **Message Formatting**：Matrix 与飞书消息格式转换
- **Web Services**：基于 Salvo 的 HTTP API

## 快速开始

### 前置要求

- Rust 1.93+
- SQLite
- Matrix homeserver（Synapse、Dendrite 等）
- 飞书应用凭据

### 安装步骤

1. **克隆并构建**
   ```bash
   git clone <repository-url>
   cd matrix-bridge-feishu
   cargo build --release
   ```

2. **生成配置模板**
   ```bash
   ./target/release/matrix-bridge-feishu --generate-config > config.yaml
   ```

3. **配置桥接参数**
   编辑 `config.yaml`，填写 Matrix 与飞书配置：
   ```yaml
   homeserver:
     address: "http://localhost:8008"
     domain: "localhost"

   bridge:
     app_id: "your_feishu_app_id"
     app_secret: "your_feishu_app_secret"
     listen_address: "http://localhost:8081"
   ```

4. **生成 Matrix registration**
   ```bash
   curl -X PUT -H "Authorization: Bearer <admin-token>" \
        -d @registration.yaml \
        "http://localhost:8008/_synapse/admin/v1/registration"
   ```

5. **启动服务**
   ```bash
   ./target/release/matrix-bridge-feishu -c config.yaml
   ```

## 配置说明

### 关键配置项

- **Homeserver Settings**：Matrix 连接信息
- **Appservice Settings**：监听地址、端口、数据库
- **Bridge Settings**：飞书应用凭据与 webhook 配置
- **Encryption Settings**：飞书 encrypt key 与 verification token
- **Permissions**：用户权限与访问控制
- **Message Settings**：消息格式转换与媒体选项
- **Message Policy Controls**：单房间限流、消息类型阻断、文本长度策略、失败降级模板
- **User Identity Sync**：`user_sync_interval_secs` 与 `user_mapping_stale_ttl_hours`

### 环境变量覆盖

可通过环境变量覆盖关键配置：

```bash
export CONFIG_PATH="/etc/matrix-bridge-feishu/config.yaml"
export MATRIX_BRIDGE_FEISHU_DB_TYPE="sqlite"
export MATRIX_BRIDGE_FEISHU_BRIDGE_APP_SECRET="real-secret"
export MATRIX_BRIDGE_FEISHU_DB_URI="sqlite:matrix-feishu.db"
export MATRIX_BRIDGE_FEISHU_AS_TOKEN="real_as_token"
```

## 飞书侧配置

1. **创建飞书应用**
   - 进入飞书开放平台
   - 创建自建应用
   - 获取 App ID 与 App Secret

2. **配置 Webhook**
   - 回调地址设为 `http://your-server:8081/webhook`
   - 开启消息与事件订阅

3. **配置权限**
   - 消息发送/接收权限
   - 用户信息读取权限
   - 文件/图片上传权限
   - 富文本与卡片相关权限

4. **可选加密配置**
   - 配置 encrypt key
   - 配置 verification token

## API 端点

### Matrix Appservice API

- `/_matrix/app/*`：Matrix appservice 端点
- `/health`：健康检查
- `/metrics`：Prometheus 指标

### 飞书 Webhook

- `/webhook`：接收飞书事件回调

### Provisioning 鉴权

Provisioning 接口默认要求 Bearer Token：

```bash
Authorization: Bearer <token>
```

- 只读接口：`MATRIX_BRIDGE_FEISHU_PROVISIONING_READ_TOKEN`
- 写入接口：`MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN`
- 删除/高风险接口：`MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN`
- 兼容回退：`MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN` / `MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN`

### Provisioning / 运维接口

- `GET /admin/status`：运行状态与 dead-letter 统计
- `GET /admin/mappings`：桥接映射列表
- `POST /admin/dead-letters/replay`：按状态与数量批量回放
- `POST /admin/dead-letters/cleanup`：按状态与时间窗口清理

### 运维 CLI 命令

```bash
./target/release/matrix-bridge-feishu -c config.yaml status
./target/release/matrix-bridge-feishu -c config.yaml mappings --limit 50 --offset 0
./target/release/matrix-bridge-feishu -c config.yaml replay --id 123
./target/release/matrix-bridge-feishu -c config.yaml replay --status pending --limit 20
./target/release/matrix-bridge-feishu -c config.yaml dead-letter-cleanup --status replayed --older-than-hours 72 --limit 500 --dry-run
```

可配合 `--admin-api http://host:port/admin` 与 `--token <provisioning_token>` 操作远端实例。

## 数据库

当前桥接存储层采用 SQLite。

### 迁移

服务启动时会自动执行数据库迁移。

## 开发指南

### 构建与检查

```bash
# Debug 构建
cargo build

# Release 构建
cargo build --release

# 运行测试
cargo test

# 格式检查
cargo fmt --check

# Lint 检查
cargo clippy
```

### 项目结构

```text
src/
├── main.rs              # 入口
├── config/              # 配置处理
├── database/            # 数据库层与迁移
├── feishu/              # 飞书 API 集成
├── bridge/              # 核心桥接逻辑
├── formatter/           # 消息格式转换
└── util/                # 通用工具
```

### 新增功能建议流程

1. 在 `feishu/` 增加平台集成代码
2. 在 `formatter/` 完善消息转换
3. 若有持久化变化，增加数据库迁移
4. 更新配置与文档

## 部署

### Docker

```dockerfile
FROM rust:1.93 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates
COPY --from=builder /app/target/release/matrix-bridge-feishu /usr/local/bin/
EXPOSE 8080 8081
CMD ["matrix-bridge-feishu", "-c", "/config/config.yaml"]
```

### Docker Compose

```yaml
version: '3.8'
services:
  matrix-bridge-feishu:
    build: .
    ports:
      - "8080:8080"
      - "8081:8081"
    volumes:
      - ./config.yaml:/config/config.yaml
      - ./data:/data
    environment:
      - RUST_LOG=info
```

### Systemd

```ini
[Unit]
Description=Matrix Appservice Feishu
After=network.target

[Service]
Type=simple
User=matrix
ExecStart=/usr/local/bin/matrix-bridge-feishu -c /etc/matrix-bridge-feishu/config.yaml
Restart=always

[Install]
WantedBy=multi-user.target
```

### 生产部署模板

推荐目录结构：

```text
/opt/matrix-bridge-feishu/
  bin/matrix-bridge-feishu
  config/config.yaml
  data/matrix-feishu.db
  logs/
```

推荐服务级环境变量：

```bash
CONFIG_PATH=/opt/matrix-bridge-feishu/config/config.yaml
MATRIX_BRIDGE_FEISHU_PROVISIONING_READ_TOKEN=<read-token>
MATRIX_BRIDGE_FEISHU_PROVISIONING_WRITE_TOKEN=<write-token>
MATRIX_BRIDGE_FEISHU_PROVISIONING_DELETE_TOKEN=<delete-token>
FEISHU_API_MAX_RETRIES=2
FEISHU_API_RETRY_BASE_MS=250
RUST_LOG=info
```

发布前建议门禁：

```powershell
pwsh ./scripts/release-check.ps1 `
  -ConfigPath ./config.yaml `
  -SubscribedEvents "im.message.receive_v1,im.message.recalled_v1,im.chat.member.user.added_v1,im.chat.member.user.deleted_v1,im.chat.updated_v1,im.chat.disbanded_v1"
```

## 监控

### 健康检查

- `/health`：HTTP 健康检查
- `/metrics`：Prometheus 指标
- 可结合 systemd 或 Docker 进行进程监控
- 建议持续监控数据库连通性

### 日志

项目使用 `tracing` 结构化日志：

```bash
# Debug 级别
RUST_LOG=debug ./matrix-bridge-feishu -c config.yaml

# 输出到文件
RUST_LOG=info ./matrix-bridge-feishu -c config.yaml > bridge.log
```

### 压测

内置脚本用于评估 webhook 突发与队列稳定性：

```powershell
pwsh ./scripts/stress-webhook.ps1 `
  -WebhookUrl http://127.0.0.1:38081/webhook `
  -MetricsUrl http://127.0.0.1:8080/metrics `
  -VerificationToken <token> `
  -SigningSecret <listen_secret>

pwsh ./scripts/stress-batch-messages.ps1 `
  -WebhookUrl http://127.0.0.1:38081/webhook `
  -MetricsUrl http://127.0.0.1:8080/metrics `
  -VerificationToken <token> `
  -SigningSecret <listen_secret>
```

两类脚本默认容量边界规则：

- `error_rate <= 1%`
- `p95 latency <= 1500ms`
- `bridge_queue_depth_max` 不应快于负载增长（webhook 并发模式下 `delta <= concurrency*2`）

推荐生产初始参数（按压测输出微调）：

- 并发：稳定上限的 `70%`
- 重试：`FEISHU_API_MAX_RETRIES=2`，`FEISHU_API_RETRY_BASE_MS=250`（接近边界可调至 `500ms`）
- 超时：`bridge.webhook_timeout = max(30s, ceil(p99*3))`，`bridge.api_timeout = max(60s, ceil(p99*4))`

## 故障排查

### 常见问题

1. **连接失败**：检查网络与防火墙策略
2. **鉴权失败**：核对飞书应用凭据
3. **数据库异常**：检查数据库连接与权限
4. **消息未桥接**：核对 webhook 配置与地址
5. **加密错误**：核对 encrypt key 与 verification token

### 10 分钟排障流程

1. 检查存活与鉴权：`/health`、`/admin/status`、`/admin/mappings`
2. 检查队列压力：`bridge_queue_depth`、`bridge_queue_depth_max`、`bridge_processing_duration_ms_*`
3. 检查发送失败：`bridge_outbound_failures_total_by_api_code` 与 `trace_id` 关联日志
4. 检查 dead-letter：`/admin/dead-letters?status=pending`，抽样回放
5. 执行完整发布检查：不带 skip 参数运行 `release-check.ps1`

### 常见错误速查

| 信号 | 含义 | 处理 |
|---|---|---|
| Feishu API HTTP `401` / `403` | 凭据或 scope 问题 | 核对 `app_id/app_secret`、应用权限、租户安装状态 |
| HTTP `429` 或 Feishu `99991663`/`90013` | 命中限流 | 降低并发，增大 `FEISHU_API_RETRY_BASE_MS` |
| `tenant_access_token invalid` | Token 失效/过期 | 轮换密钥并检查服务器时钟 |
| webhook `invalid signature` / `missing signature` | 回调签名不一致 | 核对 `listen_secret` 与 `X-Lark-*` 头透传 |
| SQLite `quick_check` 非 `ok` | 数据库完整性风险 | 停服并恢复最近备份 |

### 回滚流程

1. 停止服务：`systemctl stop matrix-bridge-feishu`
2. 回滚前备份当前数据库：
   ```bash
   sqlite3 /opt/matrix-bridge-feishu/data/matrix-feishu.db ".backup '/opt/matrix-bridge-feishu/data/matrix-feishu.db.pre_rollback'"
   ```
3. 恢复上一版本二进制与配置（`bin/` + `config/`）
4. 若 schema/数据不兼容，恢复上一个可用数据库快照
5. 启动后验证：
   - `/health` 返回 `200`
   - `/admin/status` 为 `running`
   - `bridge_outbound_failures_total` 无异常飙升
6. 服务稳定后再回放 pending dead-letter

### 聚焦排查清单

- **Webhook 签名失败**：检查 `bridge.listen_secret` 与 `X-Lark-Request-Timestamp`、`X-Lark-Request-Nonce`、`X-Lark-Signature`
- **发送权限不足**：检查飞书应用消息收发、资源读取、图片/文件 API scope
- **限流突增**：查看 `/metrics` 的 `bridge_outbound_failures_total_by_api_code`，调整 `FEISHU_API_MAX_RETRIES` / `FEISHU_API_RETRY_BASE_MS`
- **策略阻断/降级**：查看 `bridge_policy_blocked_total_by_reason` 与 `bridge_degraded_events_total_by_reason`
- **全链路追踪**：查看 `bridge_trace_events_total_by_flow_status`，按 `trace_id` 关联 `matrix_event_id` / `feishu_message_id`

### Debug 模式

```bash
RUST_LOG=debug ./matrix-bridge-feishu -c config.yaml
```

## 安全建议

- **Token 安全**：使用环境变量或密钥管理服务保存凭据
- **回调加密**：生产环境启用飞书回调加密
- **网络安全**：生产环境使用 HTTPS
- **访问控制**：合理配置 Matrix 与 Provisioning 权限

## 飞书能力说明

### 富文本支持

- 文本格式（加粗、斜体、下划线）
- @ 提及
- 链接
- 行内图片

### 卡片消息

- 交互卡片转可读文本
- 按钮动作转文本链接
- 图片卡片转 Matrix 图片

### 文件处理

- 自动上传/下载
- 必要时进行媒体转换
- 文件大小限制与策略校验

## 消息类型映射

### Matrix -> 飞书

- `m.text` -> 飞书 text
- `m.notice` -> 飞书 notice
- `m.image` -> 飞书 image
- `m.file` -> 飞书 file
- `m.audio` -> 飞书 audio
- `m.video` -> 飞书 video

### 飞书 -> Matrix

- text -> `m.text`
- rich text -> 格式化 `m.text`
- image -> `m.image`
- file -> `m.file`
- audio -> `m.audio`
- video -> `m.video`
- card -> 格式化 `m.text`

## 能力矩阵

### 飞书消息类型

| `msg_type` | 状态 | 降级策略 | 代码入口 |
|---|---|---|---|
| `text` | 已支持 | 纯文本直通 | `src/feishu/service.rs:webhook_event_to_bridge_message` |
| `post` | 已支持 | 富文本块/提及/链接展平为 Matrix 可读文本 | `src/feishu/service.rs:extract_text_from_post_content` |
| `interactive` / `card` | 部分支持 | 提取标题 + 关键元素/动作文本 | `src/feishu/service.rs:extract_text_from_card_content` |
| `image` / `file` / `audio` / `media` / `sticker` | 已支持 | 附件桥接，不可解析时降级占位文本 | `src/feishu/service.rs:webhook_event_to_bridge_message` |

### 飞书事件类型

| `event_type` | 状态 | 降级策略 | 代码入口 |
|---|---|---|---|
| `im.message.receive_v1` | 已支持 | 未识别类型降级为文本 | `src/feishu/service.rs:handle_webhook` |
| `im.message.recalled_v1` | 已支持 | 未命中 mapping 时记录并跳过 | `src/bridge/feishu_bridge.rs:handle_feishu_message_recalled` |
| `im.chat.member.user.added_v1` | 已支持 | 未命中群映射时记录并跳过 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_member_added` |
| `im.chat.member.user.deleted_v1` | 已支持 | 未命中群映射时记录并跳过 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_member_deleted` |
| `im.chat.updated_v1` | 已支持 | 增量更新已有映射字段 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_updated` |
| `im.chat.disbanded_v1` | 已支持 | 未命中映射时仅清理内存缓存 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_disbanded` |

### 桥接可靠性

| 能力 | 状态 | 说明 |
|---|---|---|
| Matrix reply/edit/redaction | 已支持 | 映射到飞书 reply/update/delete API |
| Thread/topic mapping | 已支持 | 维护并桥接 `thread_id/root_id/parent_id` |
| Dead-letter + replay | 已支持 | 失败 webhook 任务可查询与回放 |
| Media dedupe cache | 已支持 | 基于内容哈希复用飞书资源 key |
| PostgreSQL/MySQL stores | 未支持 | 当前构建有意收敛为 SQLite-only |

## 飞书侧最小必备配置

1. 创建自建飞书应用并安装到目标群聊
2. 开通消息收发、消息资源读取、图片/文件上传权限
3. 配置事件订阅：receive、recalled、成员增删、群更新、群解散
4. 配置回调安全：`listen_secret`，可选 `encrypt_key + verification_token`

## 发布前检查脚本

```powershell
pwsh ./scripts/release-check.ps1 -ConfigPath ./config.yaml
pwsh ./scripts/release-check.ps1 `
  -ConfigPath ./config.yaml `
  -SubscribedEvents "im.message.receive_v1,im.message.recalled_v1,im.chat.member.user.added_v1,im.chat.member.user.deleted_v1,im.chat.updated_v1,im.chat.disbanded_v1" `
  -ScopeProbeChatId <chat_id> `
  -ScopeProbeUserId <user_id>
```

脚本覆盖四类发布风险：

- 配置一致性：URL/数据库/token/加密组合检查
- 权限检查：read/write/delete token 分级权限校验
- 事件订阅检查：飞书必需事件列表校验
- 数据健康检查：SQLite quick check + 关键表存在性/计数校验

## 贡献

1. Fork 仓库
2. 创建功能分支
3. 提交改动
4. 补充测试
5. 发起 Pull Request

### 代码规范

- 使用 `rustfmt` 格式化
- 使用 `clippy` 进行静态检查
- 补充必要文档
- 完整处理错误路径

## 许可证

本项目采用 AGPL-3.0 License，见 [LICENSE](LICENSE)。

## 支持

- **Issues**：提交缺陷与需求
- **Discussions**：社区问答与讨论
- **Documentation**：查看项目文档与 Wiki

## 致谢

- 受 matrix-appservice-discord 与 matrix-appservice-wechat 启发
- 基于 Salvo Web 框架构建
- 使用 Diesel 进行数据库操作
- 参考飞书开放平台 API 文档
