#!/bin/bash
set -euo pipefail

CERT_DIR="${1:-certs}"
mkdir -p "$CERT_DIR"

# 生成自签名证书（有效期 365 天）
openssl req -x509 -newkey rsa:4096 -keyout "$CERT_DIR/bridge.key" \
    -out "$CERT_DIR/bridge.crt" -days 365 -nodes \
    -subj "/CN=rmux-bridge" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:0.0.0.0"

chmod 600 "$CERT_DIR/bridge.key"
chmod 644 "$CERT_DIR/bridge.crt"

echo "Certificates generated:"
echo "  cert: $CERT_DIR/bridge.crt"
echo "  key:  $CERT_DIR/bridge.key"
echo ""
echo "Copy bridge.crt to the agent-ops-mcp machine for TLS verification."
