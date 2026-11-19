#!/bin/bash
# Download the arm64e dyld shared cache of an Apple Silicon macOS release (needs `ipsw`).
#   scripts/fetch-sysroot.sh [VERSION|auto] [DEVICE]
#
# VERSION "auto" (the default) picks the newest non-beta release of the host's macOS major
# version, so the guest userland matches the host kernel (and the host's /usr/lib/dyld).
# Only the shared cache is extracted from Apple's IPSW (streamed); nothing is redistributed.
#
# When the requested release is a different macOS major than the host, the build's own
# arm64e dyld is extracted too (from the BaseSystem/restore ramdisk): a cache whose libdyld
# is newer than the host dyld makes dyld abort with "compatible libdyld.dylib not found".
# In that case point MACLATOR_DYLD at the extracted copy.
#
# Env: MACLATOR_SYSROOT_ROOT (default ~/maclator-sysroot), MACLATOR_IPSW (ipsw binary),
#      MACLATOR_FETCH_DYLD=1 (extract the dyld even when the host one is compatible),
#      MACLATOR_FORCE=1 (re-download an already installed sysroot).
set -euo pipefail

VERSION=${1:-auto}
DEVICE=${2:-Mac14,2}
ROOT=${MACLATOR_SYSROOT_ROOT:-$HOME/maclator-sysroot}
HOST_VERSION=$(sw_vers -productVersion)
HOST_MAJOR=${HOST_VERSION%%.*}

log() { printf '%s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- ipsw binary discovery -------------------------------------------------------------
# Apple's CDN answers range requests with 206 but omits "Accept-Ranges", which makes the stock
# ipsw refuse to stream an IPSW ("does not support byte-ranged requests"). A build with
# ranger.HTTPRanger.DisableAcceptRangesHeaderCheck set works; prefer one if it is around.
# Order matters: $MACLATOR_IPSW, then a patched build, then whatever is on PATH.
ipsw_candidates() {
  [ -n "${MACLATOR_IPSW:-}" ] && printf '%s\n' "$MACLATOR_IPSW"
  command -v ipsw-patched 2>/dev/null || true
  [ -x "$HOME/bin/ipsw-patched" ] && printf '%s\n' "$HOME/bin/ipsw-patched"
  command -v ipsw 2>/dev/null || true
}

IPSW=
while IFS= read -r candidate; do
  if [ -x "$candidate" ]; then IPSW=$candidate; break; fi
done < <(ipsw_candidates)
[ -n "$IPSW" ] || die "ipsw not found. Install it: brew install blacktop/tap/ipsw  (or https://github.com/blacktop/ipsw/releases)"

# --- version discovery -----------------------------------------------------------------
# AppleDB knows which releases Apple still serves. Resolve "auto"/<version> to "<version> <build>".
release_lookup() { # $1 = "major:<N>" or "<version>"
  command -v python3 >/dev/null 2>&1 || return 0
  "$IPSW" download appledb --os macOS --json 2>/dev/null |
    python3 -c 'import json, sys
want = sys.argv[1]
try:
    releases = json.load(sys.stdin).get("releases") or []
except Exception:
    sys.exit(0)

def usable(r):
    return any(a.get("active_url") for a in r.get("artifacts") or [])

def stable(r):
    version = str(r.get("version") or "")
    return version != "" and "beta" not in version.lower() and "RC" not in version

if want.startswith("major:"):
    major = want.split(":", 1)[1]
    pick = [r for r in releases
            if str(r.get("version") or "").startswith(major + ".") and stable(r) and usable(r)]
    if not pick:
        sys.exit(0)
    best = max(pick, key=lambda r: tuple(int(p) if p.isdigit() else 0
                                        for p in str(r.get("version")).split(".")))
    print(best.get("version"), best.get("build") or "")
else:
    for r in releases:
        if str(r.get("version")) == want and usable(r):
            print(r.get("version"), r.get("build") or "")
            sys.exit(0)
    sys.exit(0)' "$1" || true
}

BUILD=
if [ "$VERSION" = auto ]; then
  RELEASE=$(release_lookup "major:$HOST_MAJOR")
  VERSION=${RELEASE%% *}
  BUILD=${RELEASE#* }
  if [ -n "$VERSION" ]; then
    log "host macOS $HOST_VERSION: newest $HOST_MAJOR.x release Apple still serves is $VERSION"
  else
    VERSION=$HOST_VERSION
    log "host macOS $HOST_VERSION: no $HOST_MAJOR.x release listed, trying $VERSION"
  fi
else
  log "sysroot: requested macOS $VERSION"
  RELEASE=$(release_lookup "$VERSION")
  BUILD=${RELEASE#* }
fi
VERSION_MAJOR=${VERSION%%.*}
DEST=$ROOT/${BUILD:-$VERSION}__$DEVICE

# --- download the shared cache ---------------------------------------------------------
if [ -f "$DEST/dyld_shared_cache_arm64e" ] && [ "${MACLATOR_FORCE:-0}" != 1 ]; then
  log "already installed: $DEST (MACLATOR_FORCE=1 to re-download)"
else
  log "sysroot: macOS $VERSION (device $DEVICE)"
  TMP=$(mktemp -d "${TMPDIR:-/tmp}/maclator-sysroot.XXXXXX")
  trap 'rm -rf "$TMP"' EXIT

  fetch_cache() { # $1 = ipsw binary, $2 = output dir, $3 = log file
    "$1" download ipsw --macos --version "$VERSION" --dyld --dyld-arch arm64e \
      --extract-device "$DEVICE" --confirm --output "$2" >"$3" 2>&1
  }

  OK=0
  TRIED=0
  while IFS= read -r candidate; do
    [ -x "$candidate" ] || continue
    TRIED=$((TRIED + 1))
    log "extracting with $candidate"
    if fetch_cache "$candidate" "$TMP" "$TMP/last.log"; then
      IPSW=$candidate
      OK=1
      break
    fi
    if grep -q "byte-ranged" "$TMP/last.log" 2>/dev/null; then
      log "$(basename "$candidate"): Apple's CDN does not advertise Accept-Ranges; it cannot stream IPSWs"
      continue
    fi
    sed 's/^/  /' "$TMP/last.log" >&2
    break
  done < <(ipsw_candidates)

  if [ "$OK" = 0 ]; then
    [ "$TRIED" -gt 0 ] || die "no ipsw binary found on PATH"
    log ""
    log "Every ipsw candidate failed. Stock ipsw cannot stream from Apple's CDN (206 responses"
    log "without an Accept-Ranges header). Build one that sets"
    log "ranger.HTTPRanger.DisableAcceptRangesHeaderCheck = true, install it as"
    log "~/bin/ipsw-patched, or point \$MACLATOR_IPSW at it."
    die "could not fetch the dyld shared cache"
  fi

  CACHE=$(find "$TMP" -name dyld_shared_cache_arm64e -type f | head -1)
  [ -n "$CACHE" ] || die "no dyld_shared_cache_arm64e under $TMP"
  ACTUAL_BUILD=$(sed -n 's/.*build=\([0-9A-Za-z][0-9A-Za-z]*\).*/\1/p' "$TMP/last.log" 2>/dev/null | head -1 || true)
  [ -n "$ACTUAL_BUILD" ] && DEST=$ROOT/${ACTUAL_BUILD}__$DEVICE
  mkdir -p "$ROOT"
  rm -rf "$DEST"
  mv "$(dirname "$CACHE")" "$DEST"
  log "sysroot: $DEST"
fi

# --- extract the matching dyld (only when the host dyld cannot load this cache) --------
# Same major release: the host /usr/lib/dyld is compatible, nothing to do.
# Different major: pull /usr/lib/dyld out of the BaseSystem/restore ramdisk of the same build.
im4p_payload() { # $1 = in, $2 = out; succeeds when $1 is an IM4P-wrapped payload
  local hex length
  command -v xxd >/dev/null 2>&1 || return 1
  hex=$(xxd -p -l 27 "$1" 2>/dev/null) || return 1
  [ "${hex:0:4}" = "3084" ] || return 1            # SEQUENCE, long-form length
  [ "${hex:12:8}" = "494d3450" ] || return 1       # "IM4P" at offset 6
  length=$((16#${hex:46:8}))
  [ "$length" -gt 0 ] || return 1
  tail -c +28 "$1" | head -c "$length" >"$2"
}

extract_dyld() { # copies /usr/lib/dyld into $DEST/dyld; the scratch dir is always removed
  local work=$1 mnt=$1/mnt rc=1
  mkdir -p "$mnt"
  if _extract_dyld_into "$work" "$mnt"; then rc=0; fi
  rm -rf "$work"
  return $rc
}

_extract_dyld_into() {
  local work=$1 mnt=$2 image manifest name
  "$IPSW" download ipsw --macos --version "$VERSION" --pattern '^BuildManifest\.plist$' \
    --confirm --output "$work/manifest" >"$work/manifest.log" 2>&1 || return 1
  manifest=$(find "$work/manifest" -name BuildManifest.plist -type f | head -1)
  [ -n "$manifest" ] || return 1
  # BaseSystem (the recovery OS) and the restore ramdisk both carry /usr/lib/dyld.
  name=$(python3 -c 'import plistlib, sys
identities = plistlib.load(open(sys.argv[1], "rb")).get("BuildIdentities") or []
for key in ("BaseSystem", "RestoreRamDisk"):
    for identity in identities:
        path = (identity.get("Manifest", {}).get(key) or {}).get("Info", {}).get("Path")
        if path:
            print(path.rsplit("/", 1)[-1])
            sys.exit(0)
sys.exit(1)' "$manifest") || return 1
  log "  image: $name"
  "$IPSW" download ipsw --macos --version "$VERSION" --pattern "^${name}\$" \
    --confirm --output "$work/image" >"$work/image.log" 2>&1 || return 1
  image=$(find "$work/image" -name "$name" -type f | head -1)
  [ -n "$image" ] || return 1
  if im4p_payload "$image" "$work/raw.img"; then
    hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mnt" \
      -imagekey diskimage-class=CRawDiskImage "$work/raw.img" >/dev/null 2>&1 || return 1
  else
    hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mnt" "$image" >/dev/null 2>&1 || return 1
  fi
  local rc=1
  if [ -f "$mnt/usr/lib/dyld" ] && lipo -archs "$mnt/usr/lib/dyld" 2>/dev/null | grep -qw arm64e; then
    install -m 755 "$mnt/usr/lib/dyld" "$DEST/dyld" && rc=0
  fi
  hdiutil detach "$mnt" >/dev/null 2>&1 || true
  return $rc
}

if [ "$VERSION_MAJOR" = "$HOST_MAJOR" ] && [ "${MACLATOR_FETCH_DYLD:-0}" != 1 ]; then
  log "dyld:    host /usr/lib/dyld is compatible with this cache"
else
  log "extracting the matching arm64e dyld from this build"
  if extract_dyld "$(mktemp -d "${TMPDIR:-/tmp}/maclator-dyld.XXXXXX")"; then
    log "dyld:    $DEST/dyld"
    log ""
    log "Point maclator at it (the host dyld cannot load a newer cache):"
    log "  export MACLATOR_DYLD=\"$DEST/dyld\""
  else
    log "warning: no matching dyld found; maclator will use the host /usr/lib/dyld"
  fi
fi

log ""
log "done. Next: maclator ./some-arm64-binary"
