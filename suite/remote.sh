#!/bin/sh
# Put the rig on every box we publish numbers from, over ssh.
#
# The boxes are named in ~/.ssh/config and not here, because the names are
# personal and the script is not. Give it host names and it copies itself over,
# provisions, builds yodb from the repository and leaves the box ready to run
# `yobench`.
#
# Windows is a real target and it is not an ssh target the way the others are.
# Neither Redis nor Valkey builds on Windows natively, which is not our problem
# to solve, so a Windows host is driven through WSL and the report says so on
# the row. That is detected rather than configured: if a plain `uname` does not
# work over ssh and `wsl -- uname` does, everything goes through wsl.
#
# Usage: suite/remote.sh host [host ...]

set -eu

: "${YO_REPO:=https://github.com/tamnd/yo.git}"
: "${BENCH_REPO:=https://github.com/tamnd/yo-bench.git}"
: "${PREFIX:=/opt/yo-bench}"

[ $# -gt 0 ] || { echo "usage: suite/remote.sh host [host ...]" >&2; exit 2; }

# Everything below builds one command string and sends it. shellcheck is right
# that $PREFIX expands here and not there, and here is where it is wanted: the
# prefix is this script's decision, not the remote shell's.
# shellcheck disable=SC2029

for host in "$@"; do
  printf '\n======== %s\n' "$host"

  if ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" 'uname -s' >/dev/null 2>&1; then
    via=""
  elif ssh -o ConnectTimeout=10 -o BatchMode=yes "$host" 'wsl -- uname -s' >/dev/null 2>&1; then
    via="wsl --"
    echo "$host: driving through WSL"
  else
    echo "$host: no shell over ssh and no wsl either, skipping" >&2
    continue
  fi

  # The provisioning script goes over the wire rather than being fetched, so a
  # box can be set up from a working copy that has not been pushed yet. `cat`
  # rather than scp because scp cannot reach inside WSL. Everything else comes
  # from git, because the builds are long and a box should be able to redo them
  # without this laptop being awake.
  ssh "$host" "$via sudo mkdir -p '$PREFIX/suite'"
  ssh "$host" "$via sudo sh -c 'cat > $PREFIX/suite/provision.sh'" < suite/provision.sh

  ssh "$host" "$via sudo sh -eu -c '
    chmod +x $PREFIX/suite/provision.sh
    $PREFIX/suite/provision.sh

    export PATH=\$HOME/.cargo/bin:\$PATH

    rm -rf $PREFIX/yo $PREFIX/yo-bench
    git clone -q $YO_REPO $PREFIX/yo
    git clone -q $BENCH_REPO $PREFIX/yo-bench

    cd $PREFIX/yo && cargo build --release -p yo-cli
    cp target/release/yodb $PREFIX/bin/yodb

    cd $PREFIX/yo-bench && cargo build --release
    cp target/release/yobench $PREFIX/bin/yobench

    $PREFIX/bin/yodb --version
  '"
done

printf '\nready. Run the plan on a box with:\n'
printf '  ssh HOST %s/bin/yobench gate --prefix %s\n' "$PREFIX" "$PREFIX"
