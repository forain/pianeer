#!/bin/bash
set -euo pipefail

PIANOSAMPLER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/target/release/pianosampler"

# ── 1. Disable Intel HDA codec power save (prevents 100–500 ms first-note delay) ─
HDA_PS=/sys/module/snd_hda_intel/parameters/power_save
if [ -f "$HDA_PS" ]; then
    if echo 0 > "$HDA_PS" 2>/dev/null; then
        echo "Disabled snd_hda_intel power save."
    else
        echo "Note: run 'echo 0 | sudo tee $HDA_PS' to fully eliminate first-note delay."
    fi
fi

# ── 2. Kill any previous instance ─────────────────────────────────────────────
pkill -f "$PIANOSAMPLER" 2>/dev/null || true
sleep 1

# ── 2. Start pianosampler (handles MIDI + audio connections itself) ────────────
echo "Starting pianosampler (loading samples, please wait)..."
pw-jack "$PIANOSAMPLER" > /tmp/pianosampler.log 2>&1 &

echo -n "Waiting for pianosampler..."
for i in $(seq 1 60); do
    if grep -q "Ready" /tmp/pianosampler.log 2>/dev/null; then
        echo " ready."
        break
    fi
    if grep -q "ERROR\|error\|panicked" /tmp/pianosampler.log 2>/dev/null; then
        echo " failed." >&2
        cat /tmp/pianosampler.log >&2
        exit 1
    fi
    if [ "$i" -eq 60 ]; then
        echo " timed out." >&2
        cat /tmp/pianosampler.log >&2
        exit 1
    fi
    echo -n "."
    sleep 1
done

echo ""
echo "Ready. Signal chain:"
echo "  Keystation → pianosampler (no pitch bend/mod wheel, Salamander Grand Piano) → speakers"
echo "  Log: /tmp/pianosampler.log"
