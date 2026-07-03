# Changelog

## [Unreleased]

### Added
- Bridge 请求级别日志：INFO 显示请求摘要（type/session/duration），DEBUG 显示完整请求/响应 JSON
- Bridge `--log-level` 参数（默认 `info`，支持 trace/debug/info/warn/error，可通过 `RUST_LOG` 环境变量覆盖）

## [0.1.0] — 2026-07-02

### Added
- 39 MCP 工具（38 可用 + 1 开发中 `stream_pane`）
- 3 个批量操作工具：`batch_exec`、`batch_upload`、`batch_download`（多主机并发执行/上传/下载）
- QUIC 优先 + TCP/TLS 自动降级双协议传输
- CA 签发 + 按主机独立证书的多主机 PKI 体系
- Windows/macOS/Linux 客户端原生支持
- Bridge 并发连接限制（`--max-connections`，默认 256）
- Token 认证 + JWT 支持，恒定时间比较
- SQLite 审计日志（query/stats/cleanup）
- 主机注册表（group/tag/label 过滤、broadcast_keys）
- 文件传输：QUIC 上传/下载、目录递归并发上传
- systemd 服务部署 + `just deploy` 一键部署
- `--insecure` 标志用于调试环境跳过 TLS 验证
- 审计 CLI 子命令（`audit query/stats/cleanup`）

### Fixed
- 生产代码 `unwrap()` 改为 poison-safe 模式
- Bridge QUIC handler 支持 JSON RPC 终端操作
