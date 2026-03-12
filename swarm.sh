#!/usr/bin/env bash
# Launch N agent-bridge instances in parallel for swarm testing
# Usage: ./swarm.sh [N] [TUNNEL_URL]

set -euo pipefail

N=${1:-4}
TUNNEL_URL=${2:-"wss://app.hanzo.bot/v1/tunnel"}
PIDS=()
LOG_DIR="/tmp/hanzo-swarm-$$"
mkdir -p "$LOG_DIR"

cleanup() {
    echo ""
    echo "Stopping $N agents..."
    for pid in "${PIDS[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
    wait 2>/dev/null
    echo "All agents stopped. Logs in $LOG_DIR/"
}
trap cleanup EXIT INT TERM

echo "=== Hanzo Agent Swarm ==="
echo "  Agents:    $N"
echo "  Tunnel:    $TUNNEL_URL"
echo "  Logs:      $LOG_DIR/"
echo ""

for i in $(seq 1 "$N"); do
    INSTANCE_ID="swarm-agent-${i}-$$" \
    TUNNEL_URL="$TUNNEL_URL" \
    python3 "$(dirname "$0")/agent-bridge.py" \
        > "$LOG_DIR/agent-${i}.log" 2>&1 &
    PIDS+=($!)
    echo "  Started agent $i (pid=${PIDS[-1]}, instance=swarm-agent-${i}-$$)"
done

echo ""
echo "All $N agents launched. Press Ctrl+C to stop."
echo "Tailing logs..."
echo ""

tail -f "$LOG_DIR"/agent-*.log
