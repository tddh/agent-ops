# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in agent-ops, please **do not** open a public issue.

Instead, email the maintainer directly. We will respond within 48 hours and work on a fix.

## Supported Versions

| Version | Supported          |
|---------|--------------------|
| 0.1.x   | ✅ Supported       |

## Security Model

agent-ops consists of two components connected over TLS:

1. **agent-ops-mcp** — MCP server running alongside the AI client
2. **rmux-bridge** — Bridge daemon deployed on target Linux hosts

Security assumptions:
- The TLS channel between MCP server and bridge is encrypted and authenticated
- Bridge authentication uses static tokens with constant-time comparison
- Certificates can be self-signed or CA-issued
- By default, connections without CA verification are rejected

For production deployments:
- Use a self-managed CA to sign bridge certificates
- Rotate authentication tokens regularly
- Limit bridge access via firewall to trusted IPs only
- Run bridge as a dedicated non-root user when possible
