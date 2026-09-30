#!/usr/bin/env bash
# test-macos-create.sh — end-to-end macOS CREATE smoke test for cownfs
# Usage: sudo bash test-macos-create.sh
# Requires: root (for mount/umount), Rust release build already done.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BINDIR="$SCRIPT_DIR/target/release"
IMG=/tmp/cownfs-test-$$.img
MNT=/tmp/cownfs-mnt-$$
SERVER_PID=""
PASS=0
FAIL=0

GREEN='\033[0;32m'; RED='\033[0;31m'; NC='\033[0m'
pass() { echo -e "${GREEN}PASS${NC}  $*"; PASS=$((PASS + 1)); }
fail() { echo -e "${RED}FAIL${NC}  $*"; FAIL=$((FAIL + 1)); }

cleanup() {
  echo ""
  echo "=== cleanup ==="
  if mount | grep -q "$MNT" 2>/dev/null; then
    umount "$MNT" 2>/dev/null || true
  fi
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill "$SERVER_PID" 2>/dev/null || true
  fi
  rm -f "$IMG"
  rmdir "$MNT" 2>/dev/null || true
  echo ""
  echo "=== results: $PASS passed, $FAIL failed ==="
  [[ $FAIL -eq 0 ]]
}
trap cleanup EXIT

# ---- sanity ---------------------------------------------------------------
if [[ $EUID -ne 0 ]]; then
  echo "Run with: sudo bash $0"
  exit 1
fi

echo "=== cownfs macOS CREATE smoke test ==="
echo "bindir:  $BINDIR"
echo "image:   $IMG"
echo "mount:   $MNT"
echo ""

# ---- format ---------------------------------------------------------------
echo "--- format 64 MiB image ---"
"$BINDIR/cownfs-mkfs" --size 64M "$IMG"

# ---- start server ---------------------------------------------------------
echo "--- start server (port 2049, debug RPC off) ---"
COWNFS_DEBUG_RPC="" "$BINDIR/cownfs-server" "$IMG" 127.0.0.1:2049 &
SERVER_PID=$!
sleep 0.4   # let the socket bind

if ! kill -0 "$SERVER_PID" 2>/dev/null; then
  fail "server failed to start"
  exit 1
fi
pass "server started (pid $SERVER_PID)"

# ---- mount ----------------------------------------------------------------
echo "--- mount via macOS NFS client ---"
mkdir -p "$MNT"
# vers=4.0 — use vers=4 if your macOS rejects the minor
if mount -t nfs -o vers=4.0,tcp,port=2049,resvport 127.0.0.1:/ "$MNT"; then
  pass "mount succeeded"
else
  echo "  retrying with vers=4 ..."
  if mount -t nfs -o vers=4,tcp,port=2049,resvport 127.0.0.1:/ "$MNT"; then
    pass "mount succeeded (vers=4)"
  else
    fail "mount failed"
    exit 1
  fi
fi

# ---- basic stat -----------------------------------------------------------
echo ""
echo "--- stat root ---"
if stat "$MNT" &>/dev/null; then
  pass "stat root"
else
  fail "stat root"
fi

# ---- CREATE file ----------------------------------------------------------
echo ""
echo "--- touch / CREATE file ---"
if touch "$MNT/testfile"; then
  pass "touch testfile (CREATE)"
else
  fail "touch testfile — this was the original bug"
fi

# ---- write + read-back ----------------------------------------------------
echo ""
echo "--- write 1 MiB and read back ---"
dd if=/dev/urandom bs=1024 count=1024 2>/dev/null > /tmp/cownfs-ref-$$.bin
if cp /tmp/cownfs-ref-$$.bin "$MNT/bigfile"; then
  pass "write 1 MiB"
else
  fail "write 1 MiB"
fi
if cmp /tmp/cownfs-ref-$$.bin "$MNT/bigfile"; then
  pass "read-back matches"
else
  fail "read-back mismatch"
fi
rm -f /tmp/cownfs-ref-$$.bin

# ---- mkdir ----------------------------------------------------------------
echo ""
echo "--- mkdir ---"
if mkdir "$MNT/subdir"; then
  pass "mkdir subdir"
else
  fail "mkdir subdir"
fi

# ---- CREATE file in subdir ------------------------------------------------
echo ""
echo "--- CREATE file inside subdir ---"
if echo "hello cownfs" > "$MNT/subdir/hello.txt"; then
  pass "write subdir/hello.txt"
else
  fail "write subdir/hello.txt"
fi
if [[ "$(cat "$MNT/subdir/hello.txt")" == "hello cownfs" ]]; then
  pass "read subdir/hello.txt"
else
  fail "read subdir/hello.txt"
fi

# ---- ls -------------------------------------------------------------------
echo ""
echo "--- ls mount root ---"
ls -la "$MNT"

# ---- rename ---------------------------------------------------------------
echo ""
echo "--- rename ---"
if mv "$MNT/testfile" "$MNT/testfile.renamed"; then
  pass "rename"
else
  fail "rename"
fi

# ---- rm -------------------------------------------------------------------
echo ""
echo "--- remove ---"
if rm "$MNT/testfile.renamed"; then
  pass "remove file"
else
  fail "remove file"
fi

echo ""
echo "=== done ==="
