# Matrix Appservice Feishu（中文说明）

基于 Rust 与 Salvo 的 Matrix <-> 飞书桥接服务。

## 快速开始（开发）

1. 安装 Rust 1.75 或更高版本。
2. 复制 `example-config.yaml` 为 `config.yaml` 并按需修改。
3. 填写 Matrix 与飞书应用配置（`app_id`、`app_secret`、`as_token`、`hs_token` 等）。
4. 生成示例配置：

```bash
cargo run -- --generate-config
```

5. 启动：

```bash
cargo run -- -c config.yaml
```

## 配置文件关键项

- `homeserver`: Matrix homeserver 地址、域名与兼容选项。
- `appservice`: appservice 监听地址、端口、数据库（仅 SQLite）、token。
- `bridge`: 飞书 webhook 地址/密钥、飞书应用凭据、权限与消息策略。
- `logging`: 日志级别与输出方式。

详情见 `example-config.yaml`。

可通过环境变量覆盖关键配置：

```bash
export CONFIG_PATH="/etc/matrix-bridge-feishu/config.yaml"
export MATRIX_BRIDGE_FEISHU_DB_TYPE="sqlite"
export MATRIX_BRIDGE_FEISHU_BRIDGE_APP_SECRET="real-secret"
export MATRIX_BRIDGE_FEISHU_DB_URI="sqlite:matrix-feishu.db"
export MATRIX_BRIDGE_FEISHU_AS_TOKEN="real_as_token"
```

Provisioning 接口默认启用 Bearer 鉴权：

- `MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN`：用于查询/创建。
- `MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN`：用于删除映射（高风险操作）。

### 管理/运维接口

- `GET /admin/status`：运行状态与 dead-letter 计数
- `GET /admin/mappings`：当前 Matrix/飞书映射列表
- `POST /admin/dead-letters/replay`：按状态/数量批量回放 dead-letter
- `POST /admin/dead-letters/cleanup`：按状态/时间窗口清理 dead-letter

### 运维 CLI 命令

```bash
cargo run -- -c config.yaml status
cargo run -- -c config.yaml mappings --limit 50 --offset 0
cargo run -- -c config.yaml replay --id 123
cargo run -- -c config.yaml replay --status pending --limit 20
cargo run -- -c config.yaml dead-letter-cleanup --status replayed --older-than-hours 72 --limit 500 --dry-run
```

可通过 `--admin-api http://host:port/admin` 与 `--token <provisioning_token>` 操作远端实例。

## 能力矩阵（当前）

### 飞书消息类型（`msg_type`）

| `msg_type` | 状态 | 降级策略 | 代码入口 |
|---|---|---|---|
| `text` | 已支持 | 纯文本直通 | `src/feishu/service.rs:webhook_event_to_bridge_message` |
| `post` | 已支持 | 富文本块/提及/链接展平成可读文本 | `src/feishu/service.rs:extract_text_from_post_content` |
| `interactive` / `card` | 部分支持 | 提取标题与关键元素/按钮文本 | `src/feishu/service.rs:extract_text_from_card_content` |
| `image` / `file` / `audio` / `media` / `sticker` | 已支持 | 走附件桥接，不可解析时降级占位文本 | `src/feishu/service.rs:webhook_event_to_bridge_message` |

### 飞书事件类型（`event_type`）

| `event_type` | 状态 | 降级策略 | 代码入口 |
|---|---|---|---|
| `im.message.receive_v1` | 已支持 | 未知消息类型降级为文本 | `src/feishu/service.rs:handle_webhook` |
| `im.message.recalled_v1` | 已支持 | 未命中 mapping 时记录并跳过 | `src/bridge/feishu_bridge.rs:handle_feishu_message_recalled` |
| `im.chat.member.user.added_v1` | 已支持 | 未映射群组时记录并跳过 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_member_added` |
| `im.chat.member.user.deleted_v1` | 已支持 | 未映射群组时记录并跳过 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_member_deleted` |
| `im.chat.updated_v1` | 已支持 | 仅更新收到的增量字段 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_updated` |
| `im.chat.disbanded_v1` | 已支持 | 未命中映射时仅清理内存缓存 | `src/bridge/feishu_bridge.rs:handle_feishu_chat_disbanded` |

### 桥接稳定性能力

| 能力 | 状态 | 说明 |
|---|---|---|
| Matrix 回复/编辑/撤回 | 已支持 | 对应飞书 reply/update/delete API |
| Thread/topic 映射 | 已支持 | 维护 `thread_id/root_id/parent_id` |
| dead-letter + 回放 | 已支持 | 失败 webhook 任务可查询并手工回放 |
| 媒体去重缓存 | 已支持 | 基于内容哈希复用飞书资源 key |
| PostgreSQL/MySQL stores | 未支持 | 当前版本有意收敛为 SQLite-only |

## 排障要点

- 签名失败：核对 `bridge.listen_secret` 与 `X-Lark-*` 签名头。
- 权限不足：检查飞书应用消息收发、资源读取、图片/文件上传权限。
- 频控触发：查看 `/metrics` 的失败码指标，并调节 `FEISHU_API_MAX_RETRIES` / `FEISHU_API_RETRY_BASE_MS`。

## 发布前自检脚本

```powershell
pwsh ./scripts/release-check.ps1 -ConfigPath ./config.yaml
pwsh ./scripts/release-check.ps1 -ConfigPath ./config.yaml -SkipHttpChecks
```

## 常用开发命令

```bash
cargo build
cargo test
cargo fmt --check
cargo clippy
```

## Docker

仓库已提供：

- `Dockerfile`
- `docker-compose.yml`

可按实际环境补充配置后部署。

## 许可证

AGPL-3.0-or-later，见 `LICENSE`。
