# Matrix Appservice Feishu

A Matrix bridge for Feishu (飞书), built with Rust and the Salvo web framework.

- English: `README.md`
- 中文说明: `README.zh-CN.md`

## Features

- **Bidirectional messaging** between Matrix and Feishu
- **Rich text and card message** support with proper conversion
- **File/media sharing** including images, videos, and documents
- **User and room synchronization** with proper mapping
- **Webhook-based integration** with Feishu APIs
- **High performance** built with Rust
- **SQLite-backed persistence** for bridge mappings and event state
- **Comprehensive logging** and error handling
- **Encrypted message** support with Feishu

## Architecture

The bridge is built with a modular architecture:

- **Bridge Core**: Main application logic and state management
- **Feishu Integration**: Feishu API client and webhook handling
- **Database Layer**: Diesel-based data persistence
- **Message Formatting**: Conversion between Feishu and Matrix formats
- **Web Services**: Salvo-based HTTP endpoints

## Quick Start

### Prerequisites

- Rust 1.75+
- SQLite
- Matrix homeserver (Synapse, Dendrite, etc.)
- Feishu app credentials

### Installation

1. **Clone and build:**
   ```bash
   git clone <repository-url>
   cd matrix-appservice-feishu
   cargo build --release
   ```

2. **Generate configuration:**
   ```bash
   ./target/release/matrix-appservice-feishu --generate-config > config.yaml
   ```

3. **Configure your bridge:**
   Edit `config.yaml` with your Matrix and Feishu settings:
   ```yaml
   # Matrix homeserver
   homeserver:
     address: "http://localhost:8008"
     domain: "localhost"
   
   # Feishu app credentials
   bridge:
     app_id: "your_feishu_app_id"
     app_secret: "your_feishu_app_secret"
     listen_address: "http://localhost:8081"
   ```

4. **Generate registration:**
   ```bash
   # Create Matrix appservice registration
   curl -X PUT -H "Authorization: Bearer <admin-token>" \
        -d @registration.yaml \
        "http://localhost:8008/_synapse/admin/v1/registration"
   ```

5. **Start the bridge:**
   ```bash
   ./target/release/matrix-appservice-feishu -c config.yaml
   ```

## Configuration

### Key Configuration Options

- **Homeserver Settings**: Matrix server connection details
- **Appservice Settings**: HTTP server and database configuration
- **Bridge Settings**: Feishu credentials and webhook URL
- **Encryption Settings**: Feishu encryption key and verification token
- **Permissions**: User permissions and access control
- **Message Settings**: Formatting and media handling options

### Environment Variables

You can override configuration with environment variables:
```bash
export CONFIG_PATH="/etc/matrix-bridge-feishu/config.yaml"
export MATRIX_BRIDGE_FEISHU_BRIDGE_APP_SECRET="real-secret"
export MATRIX_BRIDGE_FEISHU_DB_URI="sqlite:matrix-feishu.db"
export MATRIX_BRIDGE_FEISHU_AS_TOKEN="real_as_token"
```

## Feishu Setup

1. **Create Feishu App:**
   - Go to Feishu Open Platform
   - Create a new application
   - Get App ID and App Secret

2. **Configure Webhooks:**
   - Set webhook URL to `http://your-server:8081/webhook`
   - Configure message events and event subscriptions

3. **Set Permissions:**
   - Message sending and receiving
   - User information access
   - File upload permissions
   - Rich text and card permissions

4. **Encryption (Optional):**
   - Configure encryption key for secure webhook messages
   - Set verification token for additional security

## API Endpoints

### Matrix Appservice API
- `/_matrix/app/*` - Matrix appservice endpoints
- `/health` - Health check endpoint
- `/metrics` - Prometheus metrics endpoint

### Feishu Webhook
- `/webhook` - Receives messages from Feishu

### Provisioning Security

Provisioning endpoints require bearer token authentication:

```bash
Authorization: Bearer <token>
```

- Read/Create endpoints use `MATRIX_BRIDGE_FEISHU_PROVISIONING_TOKEN` (defaults to `appservice.as_token`).
- Delete endpoints require `MATRIX_BRIDGE_FEISHU_PROVISIONING_ADMIN_TOKEN` (defaults to provisioning token).

## Database

The bridge currently uses SQLite for bridge store persistence.

### Migrations

Database migrations are applied automatically on startup.

## Development

### Building

```bash
# Debug build
cargo build

# Release build
cargo build --release

# Run tests
cargo test

# Check formatting
cargo fmt --check

# Run clippy
cargo clippy
```

### Project Structure

```
src/
├── main.rs              # Application entry point
├── config/              # Configuration handling
├── database/            # Database layer and migrations
├── feishu/              # Feishu API integration
├── bridge/              # Core bridge logic
├── formatter/           # Message format conversion
└── util/                # Utility functions
```

### Adding Features

1. Add platform integration in `feishu/`
2. Update message formatters in `formatter/`
3. Add database migrations if needed
4. Update configuration options

## Deployment

### Docker

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates
COPY --from=builder /app/target/release/matrix-appservice-feishu /usr/local/bin/
EXPOSE 8080 8081
CMD ["matrix-appservice-feishu", "-c", "/config/config.yaml"]
```

### Docker Compose

```yaml
version: '3.8'
services:
  matrix-appservice-feishu:
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
ExecStart=/usr/local/bin/matrix-appservice-feishu -c /etc/matrix-appservice-feishu/config.yaml
Restart=always

[Install]
WantedBy=multi-user.target
```

## Monitoring

### Health Checks

- HTTP health endpoint at `/health`
- Prometheus metrics endpoint at `/metrics`
- Process monitoring with systemd or Docker
- Database connection monitoring

### Logging

The bridge uses structured logging with `tracing`:
```bash
# Set log level
RUST_LOG=debug ./matrix-appservice-feishu -c config.yaml

# Log to file
RUST_LOG=info ./matrix-appservice-feishu -c config.yaml > bridge.log
```

## Troubleshooting

### Common Issues

1. **Connection Failed**: Check network and firewall settings
2. **Authentication Error**: Verify Feishu app credentials
3. **Database Error**: Check database connection and permissions
4. **Message Not Bridged**: Check webhook configuration and URL
5. **Encryption Error**: Verify encryption key and verification token

### Focused Playbook

- **Webhook signature failed**: verify `bridge.listen_secret` and request headers `X-Lark-Request-Timestamp`, `X-Lark-Request-Nonce`, `X-Lark-Signature`.
- **Permission denied on send**: check Feishu app scopes for message send/read and file/image APIs.
- **Rate limit spikes**: monitor `/metrics` fields `bridge_outbound_failures_total_by_api_code` and tune retry env vars `FEISHU_API_MAX_RETRIES` / `FEISHU_API_RETRY_BASE_MS`.

### Debug Mode

Enable debug logging:
```bash
RUST_LOG=debug ./matrix-appservice-feishu -c config.yaml
```

## Security

- **Token Security**: Store secrets securely (environment variables, secret management)
- **Encryption**: Use Feishu encryption for webhooks
- **Network Security**: Use HTTPS in production
- **Access Control**: Configure proper permissions in Matrix

## Feishu Features

### Rich Text Support

The bridge supports Feishu's rich text format:
- Text formatting (bold, italic, underline)
- Mentions (@user)
- Links
- Inline images

### Card Messages

Feishu card messages are converted to Matrix messages:
- Interactive cards to formatted text
- Button actions as text links
- Image cards to Matrix images

### File Handling

- Automatic file upload and download
- Media conversion when needed
- File size limits and restrictions

## Message Types

### Matrix to Feishu
- `m.text` → Feishu text message
- `m.notice` → Feishu notice
- `m.image` → Feishu image message
- `m.file` → Feishu file message
- `m.audio` → Feishu audio message
- `m.video` → Feishu video message

### Feishu to Matrix
- Text → `m.text`
- Rich text → Formatted `m.text`
- Image → `m.image`
- File → `m.file`
- Audio → `m.audio`
- Video → `m.video`
- Card → Formatted `m.text`

## Capability Matrix

### Feishu Message Types

| `msg_type` | Status | Degrade Strategy | Code Entry |
|---|---|---|---|
| `text` | Supported | Plain text passthrough | `src/feishu/service.rs:webhook_event_to_bridge_message` |
| `post` | Supported | Flatten rich blocks/mentions/links into readable Matrix text | `src/feishu/service.rs:extract_text_from_post_content` |
| `interactive` / `card` | Partial | Extract header + key elements/actions text | `src/feishu/service.rs:extract_text_from_card_content` |
| `image` / `file` / `audio` / `media` / `sticker` | Supported | Bridge as attachments, fallback placeholder text when needed | `src/feishu/service.rs:webhook_event_to_bridge_message` |

### Feishu Event Types

| `event_type` | Status | Degrade Strategy | Code Entry |
|---|---|---|---|
| `im.message.receive_v1` | Supported | Unknown message type falls back to text | `src/feishu/service.rs:handle_webhook` |
| `im.message.recalled_v1` | Supported | Missing mapping logs and skips | `src/bridge/feishu_bridge.rs:handle_feishu_message_recalled` |
| `im.chat.member.user.added_v1` | Supported | Missing room mapping logs and skips | `src/bridge/feishu_bridge.rs:handle_feishu_chat_member_added` |
| `im.chat.member.user.deleted_v1` | Supported | Missing room mapping logs and skips | `src/bridge/feishu_bridge.rs:handle_feishu_chat_member_deleted` |
| `im.chat.updated_v1` | Supported | Partial field patch to existing mapping | `src/bridge/feishu_bridge.rs:handle_feishu_chat_updated` |
| `im.chat.disbanded_v1` | Supported | Missing mapping only clears memory cache | `src/bridge/feishu_bridge.rs:handle_feishu_chat_disbanded` |

### Bridge Reliability

| Capability | Status | Notes |
|---|---|---|
| Matrix reply/edit/redaction | Supported | Routed to Feishu reply/update/delete APIs |
| Thread/topic mapping | Supported | Stores and bridges `thread_id/root_id/parent_id` |
| Dead-letter + replay | Supported | Failed webhook tasks can be listed/replayed via provisioning API |
| Media dedupe cache | Supported | Reused Feishu uploaded key by content hash |
| Postgres stores | Not supported | Bridge stores are SQLite-only in current build |

## Required Feishu Setup

1. Create a self-built Feishu app and install it to target chats.
2. Grant scopes for message send/receive, message resource read, image/file upload.
3. Configure event subscriptions: receive, recalled, chat member added/deleted, chat updated/disbanded.
4. Configure callback security: `listen_secret` and optionally `encrypt_key + verification_token`.

## Release Check Script

Use the built-in pre-release checker:

```powershell
pwsh ./scripts/release-check.ps1 -ConfigPath ./config.yaml
pwsh ./scripts/release-check.ps1 -ConfigPath ./config.yaml -SkipHttpChecks
```

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Add tests
5. Submit a pull request

### Code Style

- Use `rustfmt` for code formatting
- Use `clippy` for linting
- Add proper documentation
- Include error handling

## License

This project is licensed under the AGPL-3.0 License - see the [LICENSE](LICENSE) file for details.

## Support

- **Issues**: Report bugs and feature requests on GitHub
- **Discussions**: Community discussions and Q&A
- **Documentation**: Check the wiki for detailed guides

## Acknowledgments

- Inspired by matrix-appservice-discord and matrix-appservice-wechat
- Built with Salvo web framework
- Uses Diesel for database operations
- Feishu Open Platform API documentation
