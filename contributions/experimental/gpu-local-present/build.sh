#!/bin/bash
set -eu
MCL_EXPERIMENT_ROOT="$(cd "$(dirname "$0")" && pwd)"
MCL_REPOSITORY_ROOT="$(cd "$MCL_EXPERIMENT_ROOT/../../.." && pwd)"
if [ "$#" -ne 1 ]; then echo "Usage: bash build.sh /absolute/new-output-directory" >&2; exit 2; fi
MCL_CANDIDATE_OUT="$1"
if [ -e "$MCL_CANDIDATE_OUT" ]; then echo "Output must not already exist" >&2; exit 2; fi
mkdir -p "$MCL_CANDIDATE_OUT/gpu"
cp "$MCL_REPOSITORY_ROOT/gpu/mclproto.h" "$MCL_REPOSITORY_ROOT/gpu/mclbridge.m" "$MCL_REPOSITORY_ROOT/gpu/mclmetal.m" "$MCL_CANDIDATE_OUT/gpu/"
(cd "$MCL_CANDIDATE_OUT" && git apply --check "$MCL_EXPERIMENT_ROOT/gpu-local-present.patch" && git apply "$MCL_EXPERIMENT_ROOT/gpu-local-present.patch")
/usr/bin/clang -arch x86_64 -dynamiclib -fobjc-arc -O2 -framework Foundation -framework Metal -framework IOSurface -install_name @rpath/libmclbridge.dylib "$MCL_CANDIDATE_OUT/gpu/mclbridge.m" -o "$MCL_CANDIDATE_OUT/libmclbridge.dylib"
/usr/bin/clang -arch arm64 -dynamiclib -fno-objc-arc -O2 -framework Foundation -framework Metal -framework IOSurface -install_name @rpath/libmclmetal.dylib "$MCL_CANDIDATE_OUT/gpu/mclmetal.m" -o "$MCL_CANDIDATE_OUT/libmclmetal.dylib"
/usr/bin/codesign -f -s - "$MCL_CANDIDATE_OUT/libmclbridge.dylib" "$MCL_CANDIDATE_OUT/libmclmetal.dylib"
