#!/usr/bin/env bash
# Verify the 11 long-lived public types each have a `# Closing` rustdoc heading
# + per-language idiom table.

set -euo pipefail

# (file_path:type_name)
TYPES=(
    "crates/tst-pipeline/src/mux_sender.rs:MuxSender"
    "crates/tst-pipeline/src/sender/mod.rs:Sender"
    "crates/tst-pipeline/src/raw_sender.rs:RawSender"
    "crates/tst-pipeline/src/demux_receiver.rs:DemuxReceiver"
    "crates/tst-pipeline/src/receiver/mod.rs:Receiver"
    "crates/tst-pipeline/src/raw_receiver.rs:RawReceiver"
    "crates/tst-pipeline/src/ext/pairing/mod.rs:Pairer"
    "crates/tst-srt/src/socket.rs:SrtSocket"
    "crates/tst-srt/src/listener.rs:SrtListener"
    "crates/tst-core/src/mpegts/mux/mod.rs:Muxer"
    "crates/tst-core/src/mpegts/demux/demuxer.rs:Demuxer"
)

missing=0
for entry in "${TYPES[@]}"; do
    file="${entry%:*}"
    type_name="${entry##*:}"

    struct_line=$(grep -n "^pub struct $type_name\b" "$file" | head -1 | cut -d: -f1 || echo 0)
    if [[ "$struct_line" == "0" ]]; then
        echo "FAIL: Could not find 'pub struct $type_name' in $file"
        missing=$((missing + 1))
        continue
    fi

    start_line=$((struct_line - 80))
    if [[ "$start_line" -lt 1 ]]; then
        start_line=1
    fi
    pre_block=$(sed -n "${start_line},${struct_line}p" "$file" || true)
    if ! echo "$pre_block" | grep -q "# Closing"; then
        echo "MISSING: $type_name in $file has no '# Closing' rustdoc heading"
        missing=$((missing + 1))
        continue
    fi
    if ! echo "$pre_block" | grep -q "Per-language idiom"; then
        echo "MISSING: $type_name in $file has '# Closing' but no 'Per-language idiom' table"
        missing=$((missing + 1))
    fi
done

if [[ $missing -gt 0 ]]; then
    echo "FAIL: $missing types missing close-contract documentation"
    exit 1
fi

echo "OK: all 11 long-lived types have close-contract rustdoc"
