#!/bin/bash
# Source-only delivery. Parent invokes this later, serially.
set -eu
NF_PROBE_ROOT="$(cd "$(dirname "$0")" && pwd)"
NF_SOURCE_ROOT="$NF_PROBE_ROOT/../native-fault"
NF_BUILD_OUT="$NF_PROBE_ROOT/build"
NF_PUBLIC_ONLY=0
if [ "$#" -gt 0 ]; then
    if [ "$#" -eq 1 ] && [ "$1" = "--public-only" ]; then
        NF_PUBLIC_ONLY=1
    else
        echo "Usage: bash build-host.sh [--public-only]" >&2
        exit 2
    fi
fi
python3 "$NF_PROBE_ROOT/source_guard.py" --verify
mkdir -p "$NF_BUILD_OUT"
/usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
    -Wall -Wextra -Werror -I "$NF_SOURCE_ROOT" \
    "$NF_PROBE_ROOT/host_fault_probe.c" "$NF_PROBE_ROOT/probe_helpers.S" \
    "$NF_SOURCE_ROOT/native_fault.c" "$NF_SOURCE_ROOT/native_fault.S" \
    -o "$NF_BUILD_OUT/host-fault-probe"
if [ "$NF_PUBLIC_ONLY" -eq 0 ]; then
    /usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
        -Wall -Wextra -Werror -I "$NF_SOURCE_ROOT" \
        "$NF_PROBE_ROOT/internal_rejection_probe.c" "$NF_PROBE_ROOT/probe_helpers.S" \
        "$NF_SOURCE_ROOT/native_fault.S" -o "$NF_BUILD_OUT/internal-rejection-probe"
    /usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
        -Wall -Wextra -Werror -I "$NF_SOURCE_ROOT" \
        "$NF_PROBE_ROOT/install_fault_probe.c" "$NF_SOURCE_ROOT/native_fault.S" \
        -o "$NF_BUILD_OUT/install-fault-probe"
    python3 "$NF_PROBE_ROOT/source_guard.py" --build-info "$NF_BUILD_OUT"
else
    python3 "$NF_PROBE_ROOT/source_guard.py" --build-info "$NF_BUILD_OUT" --public-only
fi
