#!/bin/bash
# 在目标 Linux 主机上部署 rmux-bridge + rmux daemon
set -euo pipefail

BRIDGE_BINARY="${1:-./target/x86_64-unknown-linux-musl/release/rmux-bridge}"
REMOTE_HOST="${2:?Usage: $0 <bridge-binary> <user@host>}"
REMOTE_DIR="/opt/agent-ops"
BRIDGE_TOKEN="${BRIDGE_TOKEN:-$(openssl rand -hex 32)}"

echo "=== Deploying rmux-bridge to $REMOTE_HOST ==="

# 1. 安装 rmux daemon（如果未安装）
ssh "$REMOTE_HOST" 'command -v rmux || curl -fsSL https://rmux.io/install.sh | sh'

# 2. 创建目录
ssh "$REMOTE_HOST" "sudo mkdir -p $REMOTE_DIR/certs && sudo chown \$USER:\$USER $REMOTE_DIR"

# 3. 上传 bridge 二进制
scp "$BRIDGE_BINARY" "$REMOTE_HOST:$REMOTE_DIR/"

# 4. 生成并上传 TLS 证书（含 DNS + IP SAN）
HOST_IP=$(echo $REMOTE_HOST | cut -d@ -f2 | cut -d: -f1)
openssl req -x509 -newkey rsa:4096 \
    -keyout /tmp/bridge-key-$$.pem \
    -out /tmp/bridge-cert-$$.pem \
    -days 365 -nodes \
    -subj "/CN=$HOST_IP" \
    -addext "subjectAltName=DNS:$HOST_IP,IP:$HOST_IP"

scp /tmp/bridge-cert-$$.pem "$REMOTE_HOST:$REMOTE_DIR/certs/bridge.crt"
scp /tmp/bridge-key-$$.pem "$REMOTE_HOST:$REMOTE_DIR/certs/bridge.key"
rm -f /tmp/bridge-cert-$$.pem /tmp/bridge-key-$$.pem

# 5. 写入 token 到环境文件（避免 shell 注入）
ssh "$REMOTE_HOST" "echo 'BRIDGE_AUTH_TOKEN=$BRIDGE_TOKEN' | sudo tee $REMOTE_DIR/bridge.env > /dev/null && sudo chmod 600 $REMOTE_DIR/bridge.env"

# 6. 检测正确的 rmux socket 路径
RMUX_SOCK=$(ssh "$REMOTE_HOST" "ls /tmp/rmux-*/default 2>/dev/null | head -1 || echo '/tmp/rmux-0/default'")
echo "Detected rmux socket: $RMUX_SOCK"

# 7. 创建 systemd service（使用检测到的 socket 路径）
ssh "$REMOTE_HOST" "sudo tee /etc/systemd/system/rmux-bridge.service" <<SERVICE_EOF
[Unit]
Description=RMUX Bridge - TCP/TLS to Unix socket proxy
After=network.target

[Service]
Type=simple
EnvironmentFile=/opt/agent-ops/bridge.env
ExecStart=/opt/agent-ops/rmux-bridge \\
    --listen-addr 0.0.0.0:9778 \\
    --rmux-socket $RMUX_SOCK \\
    --tls-cert /opt/agent-ops/certs/bridge.crt \\
    --tls-key /opt/agent-ops/certs/bridge.key
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
SERVICE_EOF

# 7. 启动服务
ssh "$REMOTE_HOST" "sudo systemctl daemon-reload && sudo systemctl enable --now rmux-bridge"

echo ""
echo "=== Deployment complete ==="
echo "Host:     $REMOTE_HOST"
echo "Token:    $BRIDGE_TOKEN"
echo "Add this to config/hosts.yaml:"
echo ""
echo "  - name: $(echo $REMOTE_HOST | cut -d@ -f2 | cut -d. -f1)"
echo "    bridge_addr: $(echo $REMOTE_HOST | cut -d@ -f2):9778"
echo "    bridge_token: \"$BRIDGE_TOKEN\""
echo "    tags: []"
