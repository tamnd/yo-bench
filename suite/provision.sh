#!/bin/sh
# Build the rivals and the load generators from source, at pinned versions.
#
# Distribution packages are whatever the distribution felt like shipping, which
# on the box this was written against meant Redis 8.8.0 and Valkey 7.2.12. That
# is not a fair comparison in either direction: it is a year old on one side and
# two major versions behind on the other. Everything here is built from the
# upstream release tarball with the upstream Makefile, so the rivals are being
# measured the way their own maintainers build them.
#
# Everything lands under $PREFIX, nothing touches the system paths, and nothing
# here needs to run twice. A build that is already there is left alone.
#
# It also takes the distribution's own Redis and Valkey off the box. Leaving
# them installed means a stray `redis-server` on PATH, a system unit holding
# port 6379, and a real chance of measuring the wrong binary and publishing it.
# Pass --keep-packages to leave them alone.
#
# Usage: suite/provision.sh [--force] [--keep-packages]

set -eu

PREFIX="${PREFIX:-/opt/yo-bench}"
REDIS_VERSION="${REDIS_VERSION:-8.10.1}"
VALKEY_VERSION="${VALKEY_VERSION:-9.1.1}"
MEMTIER_VERSION="${MEMTIER_VERSION:-2.5.1}"
JOBS="${JOBS:-$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)}"

FORCE=no
PURGE=yes
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=yes ;;
    --keep-packages) PURGE=no ;;
    *) echo "provision: no such option: $arg" >&2; exit 2 ;;
  esac
done

SRC="$PREFIX/src"
mkdir -p "$SRC" "$PREFIX/bin"

say() { printf '\n== %s\n' "$1"; }

have() {
  # A build counts as done when the binary it was supposed to produce exists and
  # answers to --version. A half unpacked tarball from an interrupted run should
  # not read as success.
  [ "$FORCE" = no ] && [ -x "$1" ] && "$1" --version >/dev/null 2>&1
}

fetch() {
  url="$1"; out="$2"
  [ -f "$out" ] && return 0
  curl -fsSL --retry 3 -o "$out.part" "$url"
  mv "$out.part" "$out"
}

# ---------------------------------------------------------------- build deps

if [ "$(id -u)" = 0 ] && command -v apt-get >/dev/null 2>&1; then
  say "build dependencies"
  export DEBIAN_FRONTEND=noninteractive
  # Not fatal. A box with one broken third party repository in sources.list.d
  # still has every package this needs in the distribution archive, and dying
  # here would mean a benchmark rig that cannot be set up because of something
  # entirely unrelated to it.
  apt-get update -qq || echo "provision: apt-get update had problems, carrying on" >&2
  apt-get install -y -qq --no-install-recommends \
    build-essential pkg-config ca-certificates curl git \
    autoconf automake libtool \
    libssl-dev zlib1g-dev libevent-dev libpcre3-dev \
    linux-tools-common "linux-tools-$(uname -r)" >/dev/null 2>&1 ||
    apt-get install -y -qq --no-install-recommends \
      build-essential pkg-config ca-certificates curl git \
      autoconf automake libtool \
      libssl-dev zlib1g-dev libevent-dev libpcre3-dev >/dev/null
else
  # Without root there is nothing to install and nothing to be done about it, so
  # check for the tools instead of asking for them. A box that already has a
  # compiler provisions perfectly well as an ordinary user, and one that does not
  # should say which tool is missing rather than fail four minutes later inside
  # somebody else's Makefile.
  missing=""
  for tool in cc make curl tar git; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
  done
  if [ -n "$missing" ]; then
    echo "provision: not root and these are not installed:$missing" >&2
    echo "provision: install them, or rerun where sudo works" >&2
    exit 1
  fi
  say "not root, using the tools already on the box"
fi

# memtier is the one thing here that needs autotools, because its tarball ships
# without a configure script. Everything else builds with a plain make. A box
# without autoreconf still gives real numbers from redis-benchmark, so the build
# is skipped rather than fatal and the report says which generator is missing.
MEMTIER_OK=yes
command -v autoreconf >/dev/null 2>&1 || MEMTIER_OK=no

# ------------------------------------------------------- the packaged rivals

# Off the box. The point of this script is that the thing being measured is the
# thing that was built here, and the surest way to break that is to leave a
# second `redis-server` on PATH and a system unit already holding 6379. The
# builds under $PREFIX carry their version in the file name for the same reason.
if [ "$PURGE" = yes ] && [ "$(id -u)" = 0 ] && command -v apt-get >/dev/null 2>&1; then
  say "removing packaged redis and valkey"
  for unit in redis-server redis valkey-server valkey; do
    systemctl stop "$unit" >/dev/null 2>&1 || true
    systemctl disable "$unit" >/dev/null 2>&1 || true
  done
  apt-get purge -y -qq \
    'redis*' 'valkey*' >/dev/null 2>&1 || true
  apt-get autoremove -y -qq >/dev/null 2>&1 || true
  for stale in redis-server redis-cli redis-benchmark valkey-server valkey-cli; do
    if [ -e "/usr/bin/$stale" ]; then
      echo "provision: /usr/bin/$stale survived the purge" >&2
    fi
  done
fi

# ---------------------------------------------------------------------- redis

REDIS_BIN="$PREFIX/bin/redis-server-$REDIS_VERSION"
if have "$REDIS_BIN"; then
  say "redis $REDIS_VERSION already built"
else
  say "redis $REDIS_VERSION"
  fetch "https://github.com/redis/redis/archive/refs/tags/$REDIS_VERSION.tar.gz" \
        "$SRC/redis-$REDIS_VERSION.tar.gz"
  rm -rf "$SRC/redis-$REDIS_VERSION"
  tar -xzf "$SRC/redis-$REDIS_VERSION.tar.gz" -C "$SRC"
  # TLS is off and the bundled modules are off. Both are real Redis features and
  # neither is on the path a SET takes, so building them only buys a longer
  # build and a Rust toolchain requirement inside a C project.
  #
  # The allocator is left alone, which on Linux means jemalloc. Forcing libc
  # malloc would make the build faster and the memory column flattering to us,
  # and it would be measuring a Redis nobody runs.
  ( cd "$SRC/redis-$REDIS_VERSION" && make -j"$JOBS" BUILD_TLS=no >/dev/null )
  cp "$SRC/redis-$REDIS_VERSION/src/redis-server" "$REDIS_BIN"
  cp "$SRC/redis-$REDIS_VERSION/src/redis-cli" "$PREFIX/bin/redis-cli-$REDIS_VERSION"
  cp "$SRC/redis-$REDIS_VERSION/src/redis-benchmark" "$PREFIX/bin/redis-benchmark-$REDIS_VERSION"
fi

# --------------------------------------------------------------------- valkey

VALKEY_BIN="$PREFIX/bin/valkey-server-$VALKEY_VERSION"
if have "$VALKEY_BIN"; then
  say "valkey $VALKEY_VERSION already built"
else
  say "valkey $VALKEY_VERSION"
  fetch "https://github.com/valkey-io/valkey/archive/refs/tags/$VALKEY_VERSION.tar.gz" \
        "$SRC/valkey-$VALKEY_VERSION.tar.gz"
  rm -rf "$SRC/valkey-$VALKEY_VERSION"
  tar -xzf "$SRC/valkey-$VALKEY_VERSION.tar.gz" -C "$SRC"
  ( cd "$SRC/valkey-$VALKEY_VERSION" && make -j"$JOBS" BUILD_TLS=no >/dev/null )
  cp "$SRC/valkey-$VALKEY_VERSION/src/valkey-server" "$VALKEY_BIN"
  cp "$SRC/valkey-$VALKEY_VERSION/src/valkey-cli" "$PREFIX/bin/valkey-cli-$VALKEY_VERSION"
fi

# -------------------------------------------------------------------- memtier

MEMTIER_BIN="$PREFIX/bin/memtier_benchmark-$MEMTIER_VERSION"
if have "$MEMTIER_BIN"; then
  say "memtier $MEMTIER_VERSION already built"
elif [ "$MEMTIER_OK" = no ]; then
  say "no autoreconf on this box, skipping memtier"
  echo "provision: only redis-benchmark cases can run here" >&2
else
  say "memtier_benchmark $MEMTIER_VERSION"
  fetch "https://github.com/RedisLabs/memtier_benchmark/archive/refs/tags/$MEMTIER_VERSION.tar.gz" \
        "$SRC/memtier-$MEMTIER_VERSION.tar.gz"
  rm -rf "$SRC/memtier_benchmark-$MEMTIER_VERSION"
  tar -xzf "$SRC/memtier-$MEMTIER_VERSION.tar.gz" -C "$SRC"
  ( cd "$SRC/memtier_benchmark-$MEMTIER_VERSION" \
    && autoreconf -ivf >/dev/null 2>&1 \
    && ./configure --prefix="$PREFIX" >/dev/null \
    && make -j"$JOBS" >/dev/null )
  cp "$SRC/memtier_benchmark-$MEMTIER_VERSION/memtier_benchmark" "$MEMTIER_BIN"
fi

# ----------------------------------------------------------------------- rust

if ! command -v cargo >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
  say "rust"
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/dev/null
fi

# ---------------------------------------------------------------------- report

say "installed"
for b in "$REDIS_BIN" "$VALKEY_BIN" "$MEMTIER_BIN" \
         "$PREFIX/bin/redis-benchmark-$REDIS_VERSION"; do
  if [ -x "$b" ]; then
    printf '  %s\n' "$("$b" --version 2>&1 | head -1)"
  else
    printf '  %s is not here\n' "$(basename "$b")"
  fi
done
printf '\nPREFIX=%s\n' "$PREFIX"
