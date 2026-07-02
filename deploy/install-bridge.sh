#!/bin/bash
# 在目标 Linux 主机上部署 rmux-bridge + rmux daemon
set -euo pipefail

BRIDGE_BINARY="${1:?Usage: $0 <bridge-binary> <user@host> [<certs-dir>]}"
REMOTE_HOST="${2:?Usage: $0 <bridge-binary> <user@host> [<certs-dir>]}"
CERTS_DIR="${3:-certs}"
REMOTE_DIR="/opt/agent-ops"
BRIDGE_TOKEN="${BRIDGE_TOKEN:-$(openssl rand -hex 32)}"

HOST_IP=$(echo "$REMOTE_HOST" | cut -d@ -f2 | cut -d: -f1)
HOST_CERT="$CERTS_DIR/${HOST_IP}.crt"
HOST_KEY="$CERTS_DIR/${HOST_IP}.key"

echo "=== Deploying rmux-bridge to $REMOTE_HOST ==="

# 0. 检查证书
if [ ! -f "$HOST_CERT" ] || [ ! -f "$HOST_KEY" ]; then
    echo "ERROR: Certificate not found for $HOST_IP"
    echo "  Run: bash deploy/generate-certs.sh $CERTS_DIR $HOST_IP"
    echo "  Then re-run deploy."
    exit 1
fi

# 1. 安装 rmux daemon（如果未安装）
ssh "$REMOTE_HOST" 'command -v rmux || curl -fsSL https://rmux.io/install.sh | sh'

# 2. 创建目录
ssh "$REMOTE_HOST" "sudo mkdir -p $REMOTE_DIR/certs && sudo chown \$USER:\$USER $REMOTE_DIR"

# 3. 上传 bridge 二进制
scp "$BRIDGE_BINARY" "$REMOTE_HOST:$REMOTE_DIR/"

# 4. 上传主机专属的 TLS 证书
scp "$HOST_CERT" "$REMOTE_HOST:$REMOTE_DIR/certs/${HOST_IP}.crt"
scp "$HOST_KEY" "$REMOTE_HOST:$REMOTE_DIR/certs/${HOST_IP}.key"
ssh "$REMOTE_HOST" "chmod 600 $REMOTE_DIR/certs/${HOST_IP}.key"

# 5. 写入 token
ssh "$REMOTE_HOST" "echo 'BRIDGE_AUTH_TOKEN=$BRIDGE_TOKEN' | sudo tee $REMOTE_DIR/bridge.env > /dev/null && sudo chmod 600 $REMOTE_DIR/bridge.env"

# 6. 检测 rmux socket 路径
RMUX_SOCK=$(ssh "$REMOTE_HOST" "ls /tmp/rmux-*/default 2>/dev/null | head -1 || echo '/tmp/rmux-0/default'")
echo "Detected rmux socket: $RMUX_SOCK"

# 7. 创建 systemd service
ssh "$REMOTE_HOST" "sudo tee /etc/systemd/system/rmux-bridge.service" <<SERVICE_EOF
[Unit]
Description=RMUX Bridge - TCP/TLS to Unix socket proxy
After=network.target

[Service]
Type=simple
EnvironmentFile=/opt/agent-ops/bridge.env
ExecStart=/opt/agent-ops/rmux-bridge \\
    --listen-addr 0.0.0.0:9778 \\
    --quic-listen-addr 0.0.0.0:9778 \\
    --max-connections 256 \\
    --rmux-socket $RMUX_SOCK \\
    --tls-cert /opt/agent-ops/certs/${HOST_IP}.crt \\
    --tls-key /opt/agent-ops/certs/${HOST_IP}.key
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE_EOF

# 8. 启动服务
ssh "$REMOTE_HOST" "sudo systemctl daemon-reload && sudo systemctl enable --now rmux-bridge"

echo ""
echo "=== Deployment complete ==="
echo "Host:     $REMOTE_HOST"
echo "Token:    $BRIDGE_TOKEN"
echo "MCP --ca-cert:  $CERTS_DIR/ca.crt"
echo ""
echo "Add this to config/hosts.yaml:"
echo ""
echo "  - name: $(echo "$REMOTE_HOST" | cut -d@ -f2 | cut -d. -f1)"
echo "    bridge_addr: $(echo "$REMOTE_HOST" | cut -d@ -f2):9778"
echo "    bridge_token: \"$BRIDGE_TOKEN\""
echo "    tags: []"
