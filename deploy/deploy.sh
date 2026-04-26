#!/usr/bin/env bash
# Claw deployment script — run from repo root.
# Usage: ./deploy/deploy.sh [local|docker|fly]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

function print_usage() {
    echo "Usage: $(basename "$0") {local|docker|docker-rpc|fly}"
    echo ""
    echo "Targets:"
    echo "  local       Build release binary and symlink to /usr/local/bin"
    echo "  docker      Build Docker image and run 'claw doctor'"
    echo "  docker-rpc  Build Docker image for JSON-RPC server"
    echo "  fly         Deploy to Fly.io (requires flyctl)"
}

function target_local() {
    echo "=== Building local release binary ==="
    cd "${PROJECT_ROOT}/rust"
    cargo build --release -p rusty-claude-cli
    echo "Binary: ${PROJECT_ROOT}/rust/target/release/claw"
    echo "Install: sudo ln -sf ${PROJECT_ROOT}/rust/target/release/claw /usr/local/bin/claw"
}

function target_docker() {
    echo "=== Building Docker image ==="
    cd "${PROJECT_ROOT}"
    docker build -f deploy/Dockerfile -t claw:latest .
    echo ""
    echo "=== Running smoke test ==="
    docker run --rm claw:latest --version
    echo ""
    echo "=== To run the agent interactively ==="
    cat <<EOF
docker run -it --rm \\
    -v \$(pwd):/workspace/project \\
    -e ANTHROPIC_API_KEY \\
    claw:latest
EOF
}

function target_docker_rpc() {
    echo "=== Building Docker image for RPC ==="
    cd "${PROJECT_ROOT}"
    docker build -f deploy/Dockerfile -t claw-rpc:latest .
    echo ""
    echo "=== Starting RPC server on localhost:6688 ==="
    docker rm -f claw-rpc 2>/dev/null || true
    docker run -d --name claw-rpc \\n        -p 127.0.0.1:6688:6688 \\n        -v "$(pwd):/workspace/project" \\n        --env-file .env \\n        claw-rpc:latest rpc
    echo "RPC server running. Test: curl -X POST http://localhost:6688"
}

function target_fly() {
    if ! command -v flyctl >/dev/null 2>&1; then
        echo "Error: flyctl not found. Install: https://fly.io/docs/hands-on/install-flyctl/"
        exit 1
    fi
    cd "${PROJECT_ROOT}"

    if [[ ! -f deploy/fly.toml ]]; then
        echo "Error: deploy/fly.toml not found. Run 'flyctl launch' first."
        exit 1
    fi

    flyctl deploy --config deploy/fly.toml --dockerfile deploy/Dockerfile
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

case "${1:-help}" in
    local)
        target_local
        ;;
    docker|docker-local)
        target_docker
        ;;
    docker-rpc)
        target_docker_rpc
        ;;
    fly)
        target_fly
        ;;
    *)
        print_usage
        exit 1
        ;;
esac
