# Matrix Appservice Feishu（中文说明）

基于 Rust 与 Salvo 的 Matrix <-> 飞书桥接服务。

## 当前状态（2026-02-15）

此前未完成的关键链路已补齐，当前版本可编译并可运行。

- 构建依赖已修复：`cargo check` / `cargo test` 可通过。
- 已实现 Matrix appservice 事务接收与事件分发。
- 已实现飞书 webhook 接收、签名校验、URL 验证、事件解析与入桥处理。
- 已补齐 Matrix <-> 飞书消息转发入口与格式转换调用。
- 已修复数据库模块声明、类型定义与路由处理器等编译阻塞问题。
- 当前桥接存储仅支持 SQLite（`appservice.database.type` 必须为 `sqlite`）。

## 已有能力（代码层面）

- 配置加载与基础校验（YAML）。
- Appservice / bridge / formatter / feishu 模块化结构。
- SQLite 初始化与基础建表逻辑。
- 飞书 API 客户端框架（鉴权、发消息、上传图片等接口封装）。
- Matrix 与飞书消息格式转换的基础实现（文本、富文本、卡片、媒体占位等）。

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
export MATRIX_BRIDGE_FEISHU_BRIDGE_APP_SECRET="real-secret"
export MATRIX_BRIDGE_FEISHU_DB_URI="sqlite:matrix-feishu.db"
export MATRIX_BRIDGE_FEISHU_AS_TOKEN="real_as_token"
```

Provisioning 接口默认启用 Bearer 鉴权：

- `MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN`：用于查询/创建。
- `MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN`：用于删除映射（高风险操作）。

## 能力矩阵（当前）

| 能力 | 状态 | 说明 |
|---|---|---|
| `im.message.receive_v1` | 已支持 | 文本/富文本/图片/文件/音频/视频/卡片解析 |
| `im.message.recalled_v1` | 已支持 | 通过 mapping 回撤 Matrix 消息 |
| `im.chat.member.user.added_v1` | 已支持 | 同步成员加入提示与审计日志 |
| `im.chat.member.user.deleted_v1` | 已支持 | 同步成员离开提示与审计日志 |
| `im.chat.updated_v1` | 已支持 | 同步群信息与 thread 模式策略 |
| `im.chat.disbanded_v1` | 已支持 | 自动清理映射并通知 Matrix |
| Matrix 回复/编辑/撤回 | 已支持 | 对应飞书 reply/update/delete API |
| Thread/topic 映射 | 已支持 | 维护 `thread_id/root_id/parent_id` |
| PostgreSQL stores | 未支持 | 当前桥接存储仅支持 SQLite |

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
