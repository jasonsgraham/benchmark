#!/usr/bin/env bash
# run-comparison.sh — full head-to-head benchmark: FalkorDB vs Neo4j vs ibexdb
#
# Starts all three vendor databases in Docker containers, loads the pokec-small
# dataset, generates a read-only query set, runs each vendor at 40 workers /
# 4000 mps, and opens Grafana for side-by-side latency comparison.
#
# Usage:
#   ./run-comparison.sh                # fresh run: reset data, load, benchmark
#   SKIP_LOAD=1 ./run-comparison.sh    # skip load (reuse data from last run)
#   SKIP_BUILD=1 ./run-comparison.sh   # skip `cargo build` (use existing binary)
#
# Prerequisites: Docker (Compose V2), curl, nc, Rust toolchain

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

SKIP_LOAD="${SKIP_LOAD:-0}"
SKIP_BUILD="${SKIP_BUILD:-0}"

# ── helpers ───────────────────────────────────────────────────────────────────

log() { echo "==> $*"; }

wait_for_http() {
    local url=$1 name=$2
    printf "    waiting for %s" "$name"
    for _ in $(seq 1 60); do
        if curl -sf "$url" >/dev/null 2>&1; then echo " ready"; return 0; fi
        printf "."
        sleep 2
    done
    echo " TIMEOUT"; exit 1
}

wait_for_tcp() {
    local host=$1 port=$2 name=$3
    printf "    waiting for %s" "$name"
    for _ in $(seq 1 60); do
        if nc -z "$host" "$port" 2>/dev/null; then echo " ready"; return 0; fi
        printf "."
        sleep 2
    done
    echo " TIMEOUT"; exit 1
}

run_benchmark() {
    cargo run --bin benchmark -- "$@"
}

# ── 1. Vendor containers ──────────────────────────────────────────────────────

log "Stopping any running vendor containers..."
# Must stop before wiping bind-mount dirs — running containers hold graph state
# in memory and `up -d` won't recreate them just because the host dir changed.
docker compose -f docker-compose.vendors.yml down --remove-orphans

log "Resetting vendor data..."
rm -rf ./vendor-data/falkor ./vendor-data/neo4j ./vendor-data/ibex
mkdir -p ./vendor-data/falkor ./vendor-data/neo4j/data ./vendor-data/neo4j/logs ./vendor-data/ibex

log "Starting vendor containers (FalkorDB / Neo4j / ibexdb)..."
docker compose -f docker-compose.vendors.yml up -d --build

log "Waiting for vendor containers..."
wait_for_tcp 127.0.0.1 6379  "FalkorDB (Redis:6379)"
wait_for_http http://localhost:7474 "Neo4j HTTP (:7474)"
wait_for_http http://localhost:8088/api/stats "ibexdb (:8088)"

# ── 2. Metrics stack (Prometheus + Grafana) ───────────────────────────────────

log "Generating and starting metrics stack..."
./generate_docker_compose.sh
docker compose up -d
wait_for_http http://localhost:3000 "Grafana (:3000)"

# ── 3. Build benchmark harness ────────────────────────────────────────────────

if [ "$SKIP_BUILD" -eq 0 ]; then
    log "Building benchmark harness (debug)..."
    cargo build --bin benchmark
fi

# ── 4. Load data into each vendor ─────────────────────────────────────────────

if [ "$SKIP_LOAD" -eq 0 ]; then
    log "Loading pokec-small into FalkorDB..."
    FALKOR_EXTERNAL=1 \
        run_benchmark load --vendor falkor --size small --force

    log "Loading pokec-small into ibexdb..."
    IBEX_EXTERNAL=1 IBEX_ENDPOINT=http://127.0.0.1:8088 \
        run_benchmark load --vendor ibex --size small --force

    log "Loading pokec-small into Neo4j..."
    NEO4J_EXTERNAL=1 NEO4J_URI=127.0.0.1:7687 NEO4J_PASSWORD=h6u4krd10 \
        run_benchmark load --vendor neo4j --size small --force
fi

# ── 5. Generate read-only query set ───────────────────────────────────────────

log "Generating query set (small-readonly)..."
run_benchmark generate-queries \
    --size 10000000 --dataset small --name small-readonly --write-ratio 0.0

# ── 6. Run benchmarks (sequential so Grafana shows clean time-separated bands) ─

log "Running FalkorDB benchmark..."
FALKOR_EXTERNAL=1 \
    run_benchmark run --vendor falkor --name small-readonly --parallel 40 --mps 4000

log "Running ibexdb benchmark..."
IBEX_EXTERNAL=1 IBEX_ENDPOINT=http://127.0.0.1:8088 \
    run_benchmark run --vendor ibex --name small-readonly --parallel 40 --mps 4000

log "Running Neo4j benchmark..."
NEO4J_EXTERNAL=1 NEO4J_URI=127.0.0.1:7687 NEO4J_PASSWORD=h6u4krd10 \
    run_benchmark run --vendor neo4j --name small-readonly --parallel 40 --mps 4000

# ── 7. Results ────────────────────────────────────────────────────────────────

echo ""
echo "┌──────────────────────────────────────────────────────┐"
echo "│  Benchmark complete!                                 │"
echo "│  Grafana:    http://localhost:3000  (admin / admin)  │"
echo "│  Prometheus: http://localhost:9090                   │"
echo "└──────────────────────────────────────────────────────┘"

if command -v open >/dev/null 2>&1; then
    open http://localhost:3000
elif command -v xdg-open >/dev/null 2>&1; then
    xdg-open http://localhost:3000
fi
