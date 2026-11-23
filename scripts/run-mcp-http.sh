#!/usr/bin/env bash
# Run the semcode MCP Streamable HTTP server from the container image.
#
# The image is built with Dockerfile.http:
#   docker build -t semcode-mcp:local -f Dockerfile.http .
#   ./scripts/run-mcp-http.sh --repo /srv/src/linux
#
# One container serves one repository. The endpoint is
# http://127.0.0.1:<port>/mcp and is published to host loopback only.
set -euo pipefail

IMAGE="${SEMCODE_MCP_IMAGE:-semcode-mcp:local}"
PORT="${SEMCODE_MCP_PORT:-8765}"
REPO=""
DB=""
NAME=""
DETACH=0
NO_AUTO_INDEX=0
LAZY=0
TOKEN_FILE=""
MODEL_CACHE="${SEMCODE_MODEL_CACHE:-${HOME}/.cache/semcode/models}"
PUBLISH_ADDR="127.0.0.1"
BUILD=0
EXTRA=()

usage() {
    cat <<'EOF'
Usage: run-mcp-http.sh --repo <path> [options]

Required:
  --repo <path>        Git repository that was indexed (mounted read-only at /repo)

Options:
  --db <path>          Index directory (default: <repo>/.semcode.db)
  --port <port>        Published port (default: 8765, or $SEMCODE_MCP_PORT)
  --name <name>        Container name (default: semcode-mcp-<repo basename>)
  --image <image>      Image to run (default: semcode-mcp:local)
  --token-file <path>  Host file with the Bearer token, mounted at /run/secrets/semcode-token
  --model-cache <dir>  Host model cache (default: ~/.cache/semcode/models)
  --publish <addr>     Host address to publish on (default: 127.0.0.1;
                       use 0.0.0.0 for LAN access, and remember the token)
  --no-auto-index      Pass --no-auto-index to the server
  --lazy               Pass --lazy to the server
  -d, --detach         Run in the background with --restart unless-stopped
  --build              Build the image before running (Dockerfile.http)
  -- <args...>         Extra arguments passed to semcode-mcp verbatim
  -h, --help           Show this help

Authentication:
  Set SEMCODE_MCP_TOKEN in the environment to pass it to the container, or use
  --token-file. With neither, the endpoint is unauthenticated (the server logs
  a warning). The token is never placed on the command line.
EOF
}

die() { echo "error: $*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) REPO="${2:?--repo needs a value}"; shift 2 ;;
        --db) DB="${2:?--db needs a value}"; shift 2 ;;
        --port) PORT="${2:?--port needs a value}"; shift 2 ;;
        --name) NAME="${2:?--name needs a value}"; shift 2 ;;
        --image) IMAGE="${2:?--image needs a value}"; shift 2 ;;
        --token-file) TOKEN_FILE="${2:?--token-file needs a value}"; shift 2 ;;
        --model-cache) MODEL_CACHE="${2:?--model-cache needs a value}"; shift 2 ;;
        --publish) PUBLISH_ADDR="${2:?--publish needs a value}"; shift 2 ;;
        --no-auto-index) NO_AUTO_INDEX=1; shift ;;
        --lazy) LAZY=1; shift ;;
        -d|--detach) DETACH=1; shift ;;
        --build) BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; EXTRA+=("$@"); break ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

command -v docker >/dev/null || die "docker is not installed"
[[ -n "$REPO" ]] || die "--repo is required (try --help)"
[[ -d "$REPO" ]] || die "repository does not exist: $REPO"
REPO="$(cd "$REPO" && pwd)"
[[ "$PORT" =~ ^[0-9]+$ ]] && (( PORT >= 1 && PORT <= 65535 )) || die "invalid port: $PORT"

DB="${DB:-$REPO/.semcode.db}"
[[ -d "$DB" ]] || die "index not found: $DB (run 'semcode-index -s \"$REPO\"' first)"
DB="$(cd "$DB" && pwd)"

if [[ -z "$NAME" ]]; then
    base="$(basename "$REPO" | tr -c 'A-Za-z0-9_.-' '-')"
    NAME="semcode-mcp-${base:-repo}"
fi

if [[ $BUILD -eq 1 ]]; then
    script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    root_dir="$(cd "$script_dir/.." && pwd)"
    docker build -t "$IMAGE" -f "$root_dir/Dockerfile.http" "$root_dir"
fi

docker image inspect "$IMAGE" >/dev/null 2>&1 \
    || die "image not found: $IMAGE (build it with 'docker build -t $IMAGE -f Dockerfile.http .')"

if docker container inspect "$NAME" >/dev/null 2>&1; then
    die "container '$NAME' already exists (remove it with 'docker rm -f $NAME' or choose --name)"
fi

mkdir -p "$MODEL_CACHE"
MODEL_CACHE="$(cd "$MODEL_CACHE" && pwd)"

server_args=(
    --transport http
    --bind 0.0.0.0
    --port "$PORT"
    --database /data/.semcode.db
    --git-repo /repo
)
[[ $NO_AUTO_INDEX -eq 1 ]] && server_args+=(--no-auto-index)
[[ $LAZY -eq 1 ]] && server_args+=(--lazy)

run_args=(
    --name "$NAME"
    --init
    --security-opt no-new-privileges:true
    --cap-drop ALL
    --user "$(id -u):$(id -g)"
    -e HOME=/home/semcode
    -p "${PUBLISH_ADDR}:${PORT}:${PORT}"
    -v "${REPO}:/repo:ro"
    -v "${DB}:/data/.semcode.db"
    -v "${MODEL_CACHE}:/home/semcode/.cache/semcode/models"
)

if [[ -n "$TOKEN_FILE" ]]; then
    [[ -f "$TOKEN_FILE" ]] || die "token file not found: $TOKEN_FILE"
    TOKEN_FILE="$(cd "$(dirname "$TOKEN_FILE")" && pwd)/$(basename "$TOKEN_FILE")"
    run_args+=(-v "${TOKEN_FILE}:/run/secrets/semcode-token:ro")
    server_args+=(--token-file /run/secrets/semcode-token)
elif [[ -n "${SEMCODE_MCP_TOKEN:-}" ]]; then
    run_args+=(-e SEMCODE_MCP_TOKEN)
else
    echo "warning: no token configured; the endpoint is unauthenticated" >&2
fi

if [[ ${#EXTRA[@]} -gt 0 ]]; then
    server_args+=("${EXTRA[@]}")
fi

if [[ $DETACH -eq 1 ]]; then
    # `--restart` and `--rm` are mutually exclusive in Docker.
    run_args+=(-d --restart unless-stopped)
else
    run_args+=(--rm)
fi

echo "image:      $IMAGE"
echo "repository: $REPO (read-only)"
echo "index:      $DB (writeable; the current commit may be indexed at startup)"
echo "endpoint:   http://127.0.0.1:${PORT}/mcp"

if [[ $DETACH -eq 1 ]]; then
    docker run "${run_args[@]}" "$IMAGE" "${server_args[@]}"
    echo "started container '$NAME'; follow logs with: docker logs -f $NAME"
else
    exec docker run "${run_args[@]}" "$IMAGE" "${server_args[@]}"
fi