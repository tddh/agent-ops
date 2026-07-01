# agent-ops build commands

default: check

# ─── 编译 ────────────────────────────
check:
    cargo check --workspace

build:
    cargo build --workspace

release:
    cargo build --workspace --release

# 交叉编译 Linux x86_64
release-linux:
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc cargo build --target x86_64-unknown-linux-musl --release -p rmux-bridge
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc cargo build --target x86_64-unknown-linux-musl --release -p agent-ops-mcp

check-bridge:
    cargo check -p rmux-bridge

check-mcp:
    cargo check -p agent-ops-mcp

build-bridge:
    cargo build -p rmux-bridge --release

build-mcp:
    cargo build -p agent-ops-mcp --release

# ─── 运行 ────────────────────────────
run-bridge token:
    BRIDGE_AUTH_TOKEN="{{token}}" cargo run -p rmux-bridge -- \
        --listen-addr 127.0.0.1:19778 --auth-token "{{token}}"

run-mcp hosts='config/hosts.yaml':
    cargo run -p agent-ops-mcp -- --hosts-file {{hosts}}

# ─── 测试 ────────────────────────────
test:
    cargo test --workspace

# ─── 代码质量 ────────────────────────
fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace -- -D warnings

# ─── 清理 ────────────────────────────
clean:
    cargo clean

# ─── 证书 ────────────────────────────
certs:
    bash deploy/generate-certs.sh

# ─── 部署 ────────────────────────────
deploy host token='{{token}}':
    BRIDGE_TOKEN="{{token}}" bash deploy/install-bridge.sh ./target/x86_64-unknown-linux-musl/release/rmux-bridge {{host}}
