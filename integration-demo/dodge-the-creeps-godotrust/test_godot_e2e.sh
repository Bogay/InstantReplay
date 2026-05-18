#!/usr/bin/env bash
# E2E test: launches Godot and verifies that InstantReplayRecorder produces
# a non-empty MP4 file after a short recording session.
#
# Display priority:
#   1. Real display ($DISPLAY already set)            — host desktop
#   2. xvfb-run (auto virtual framebuffer)            — CI without GPU
#   3. Manual Xvfb                                    — fallback virtual
#   4. --headless                                     — no rendered frames
#
# Requirements:
#   - godot 4.x in PATH (or override with GODOT= env var)
#   - unienc_godot GDExtension built (just build-recorder)
#   - ffmpeg in PATH
#
# Usage:
#   ./test_godot_e2e.sh [--build]
#     --build   rebuild the recorder extension before running

set -euo pipefail

SCRIPT_DIR=$(dirname "$(realpath "$0")")
PROJECT_DIR="$SCRIPT_DIR/godot"
# The test script writes next to project.godot for easy inspection
OUTPUT_FILE="$PROJECT_DIR/test_replay.mp4"
# Resolve godot binary: honour GODOT env var, then try common names/flatpak
if [[ -n "${GODOT:-}" ]]; then
    : # already set by caller
elif command -v godot &>/dev/null 2>&1; then
    GODOT="godot"
elif command -v godot4 &>/dev/null 2>&1; then
    GODOT="godot4"
elif flatpak info org.godotengine.Godot &>/dev/null 2>&1; then
    GODOT="flatpak run org.godotengine.Godot"
else
    echo "ERROR: cannot find godot binary. Set GODOT= env var or install Godot." >&2
    exit 1
fi
TIMEOUT_SECS=30

# ── Parse arguments ──────────────────────────────────────────────────────────
BUILD=0
for arg in "$@"; do
    [[ "$arg" == "--build" ]] && BUILD=1
done

# ── Cleanup ──────────────────────────────────────────────────────────────────
XVFB_PID=""
cleanup() {
    [[ -n "$XVFB_PID" ]] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

rm -f "$OUTPUT_FILE"

# ── Optional build ───────────────────────────────────────────────────────────
if [[ $BUILD -eq 1 ]]; then
    echo "[build] Building recorder extension..."
    cd "$SCRIPT_DIR"
    just build-recorder
fi

# ── Display setup ────────────────────────────────────────────────────────────
if [[ -n "${DISPLAY:-}" ]]; then
    GODOT_RUN="$GODOT"
    echo "[display] Using existing display $DISPLAY"
elif command -v xvfb-run &>/dev/null; then
    GODOT_RUN="xvfb-run --auto-servernum --server-args=-screen 0 320x240x24 $GODOT"
    echo "[display] Using xvfb-run"
elif command -v Xvfb &>/dev/null; then
    DISP=":$((RANDOM % 100 + 100))"
    Xvfb "$DISP" -screen 0 320x240x24 -nolisten tcp &
    XVFB_PID=$!
    sleep 0.5
    export DISPLAY="$DISP"
    GODOT_RUN="$GODOT"
    echo "[display] Using Xvfb on $DISP"
else
    echo "[display] WARNING: no display found; running headless (no rendered frames)"
    GODOT_RUN="$GODOT --headless"
fi

# ── Run test ─────────────────────────────────────────────────────────────────
echo "[test] Running: $GODOT_RUN --path $PROJECT_DIR --script res://test_recorder_e2e.gd"
set +e
timeout "$TIMEOUT_SECS" $GODOT_RUN \
    --path "$PROJECT_DIR" \
    --script "res://test_recorder_e2e.gd"
EXIT_CODE=$?
set -e

# ── Result ───────────────────────────────────────────────────────────────────
if [[ $EXIT_CODE -eq 124 ]]; then
    echo "[FAIL] Test timed out after ${TIMEOUT_SECS}s"
    exit 1
fi

if [[ -f "$OUTPUT_FILE" ]]; then
    SIZE=$(stat -c%s "$OUTPUT_FILE")
else
    SIZE=0
fi

if [[ $EXIT_CODE -eq 0 && $SIZE -gt 0 ]]; then
    echo "[PASS] $OUTPUT_FILE (${SIZE} bytes)"
    exit 0
else
    echo "[FAIL] exit_code=$EXIT_CODE  file_size=${SIZE}B  path=$OUTPUT_FILE"
    exit 1
fi
