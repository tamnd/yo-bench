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
# A Windows host is also why nothing here sends a shell fragment over ssh. The
# far end of an ssh command on Windows is cmd.exe, which reads `>` and `&&` and
# `|` out of the command line before wsl ever sees them, and does not know that
# single quotes were meant to protect them. A `cat > file` sent that way tries
# to write a Windows file called `/opt/yo-bench/suite/provision.sh` and says
# "The system cannot find the path specified", which is a confusing way to be
# told the quoting was eaten. So every script travels as bytes on stdin and is
# then run by name, and every command sent from here is a plain argument list
# with no operators in it.
#
# Usage: suite/remote.sh host [host ...]

set -eu

: "${YO_REPO:=https://github.com/tamnd/yo.git}"
: "${BENCH_REPO:=https://github.com/tamnd/yo-bench.git}"
: "${PREFIX:=/opt/yo-bench}"

[ $# -gt 0 ] || { echo "usage: suite/remote.sh host [host ...]" >&2; exit 2; }

failed=""

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

  # Root where we can have it and $HOME where we cannot. A box without
  # passwordless sudo is still a box worth measuring, it just cannot have its
  # packages purged or write to /opt, and refusing to provision it would mean
  # publishing three rows where four were asked for.
  #
  # The home directory is asked for rather than written as `$HOME`, because the
  # prefix ends up inside scripts and inside messages printed here, and a path
  # that only means something on the far end is no use in either.
  if ssh "$host" "$via sudo -n true" >/dev/null 2>&1; then
    sudo="sudo"
    prefix="$PREFIX"
    keep=""
  else
    sudo=""
    home=$(ssh "$host" "$via printenv HOME")
    prefix="$home/yo-bench"
    keep="--keep-packages"
    echo "$host: no passwordless sudo, installing under $prefix and leaving the packages alone"
  fi

  # The provisioning script goes over the wire rather than being fetched, so a
  # box can be set up from a working copy that has not been pushed yet. `tee`
  # rather than scp because scp cannot reach inside WSL, and rather than
  # `cat >` because of the cmd.exe problem at the top of this file.
  # One box failing is not four boxes failing. A run that stops at the first
  # broken host means the other three sit unprovisioned for no reason, so every
  # host is attempted and the failures are collected and reported at the end.
  ok=yes
  ssh "$host" "$via $sudo mkdir -p $prefix/suite" || ok=no

  # The build half is written out here and sent the same way, so that the far
  # end runs a file rather than a command line. It is also a file the box keeps,
  # which means a rebuild after a push is one ssh and not a rerun of all of this.
  job=$(mktemp)
  trap 'rm -f "$job"' EXIT INT TERM
  cat > "$job" <<JOB
#!/bin/sh
# Written by suite/remote.sh. Rerun it by hand to rebuild after a push.
set -eu

chmod +x $prefix/suite/provision.sh
PREFIX=$prefix $prefix/suite/provision.sh $keep

# Cargo installs itself into a home directory and nothing puts it on the PATH of
# a non login shell, which is what an ssh command gets.
export PATH=\$HOME/.cargo/bin:\$PATH

for pair in "yo $YO_REPO" "yo-bench $BENCH_REPO"; do
  # Fetch into a clone that is already there rather than starting again. These
  # are release builds of a large workspace and throwing the target directory
  # away every time turns a thirty second rebuild into a five minute one.
  set -- \$pair
  if [ -d "$prefix/\$1/.git" ]; then
    git -C "$prefix/\$1" fetch -q origin
    git -C "$prefix/\$1" reset -q --hard origin/HEAD
  else
    rm -rf "$prefix/\$1"
    git clone -q "\$2" "$prefix/\$1"
  fi
done

cd $prefix/yo
cargo build --release -p yo-cli
cp target/release/yodb $prefix/bin/yodb

cd $prefix/yo-bench
cargo build --release
cp target/release/yobench $prefix/bin/yobench

$prefix/bin/yodb --version
JOB

  if [ "$ok" = yes ]; then
    ssh "$host" "$via $sudo tee $prefix/suite/provision.sh" < suite/provision.sh >/dev/null || ok=no
  fi
  if [ "$ok" = yes ]; then
    ssh "$host" "$via $sudo tee $prefix/suite/build.sh" < "$job" >/dev/null || ok=no
  fi
  if [ "$ok" = yes ]; then
    ssh "$host" "$via $sudo sh -eu $prefix/suite/build.sh" || ok=no
  fi

  rm -f "$job"
  [ "$ok" = yes ] || { echo "$host: provisioning failed" >&2; failed="$failed $host"; }
done

if [ -n "$failed" ]; then
  printf '\nthese boxes did not provision:%s\n' "$failed" >&2
  exit 1
fi

printf '\nready. Run the plan on a box with:\n'
printf '  ssh HOST %s/bin/yobench gate --prefix %s\n' "$PREFIX" "$PREFIX"
printf 'and rebuild it after a push with:\n'
printf '  ssh HOST sh %s/suite/build.sh\n' "$PREFIX"
