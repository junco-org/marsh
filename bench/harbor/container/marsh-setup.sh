#!/bin/sh
# Runs as root inside a task container, before the agent starts.
#
#   marsh-setup.sh WORKDIR MODE [USER]
#
# Moves WORKDIR onto a fresh btrfs subvolume (marsh's source), leaves a symlink at WORKDIR so
# the task, the agent and the verifier keep using the same path, and installs the tmux shim.
#
# Storage, in order of preference:
#   1. /marsh-store is already btrfs: a host directory on a btrfs filesystem mounted with
#      user_subvol_rm_allowed, bind-mounted by compose/marsh.yaml. No extra privileges.
#   2. Otherwise a sparse loop image formatted here. Needs a privileged container
#      (compose/marsh-loop.yaml).
set -eu

WORKDIR=${1:?usage: marsh-setup.sh WORKDIR MODE [USER]}
MODE=${2:?usage: marsh-setup.sh WORKDIR MODE [USER]}
AGENT_USER=${3:-}
BUNDLE=/opt/marsh
STORE=/marsh-store
LOOP_SIZE=${MARSH_LOOP_SIZE:-16G}

fail() {
    echo "marsh-setup: $*" >&2
    exit 1
}
# The bundle's btrfs tools carry their own libraries; only these two commands see them.
btrfs_tool() {
    tool=$1
    shift
    LD_LIBRARY_PATH="$BUNDLE/lib" "$BUNDLE/bin/$tool" "$@"
}

[ -x "$BUNDLE/bin/rmux" ] || fail "no marsh bundle at $BUNDLE; mount it (compose/marsh.yaml)"
# Checked before resolving: a moved WORKDIR is the symlink setup leaves behind.
if [ -L "${WORKDIR%/}" ]; then
    fail "$WORKDIR was already moved onto marsh storage"
fi
WORKDIR=$(cd "$WORKDIR" && pwd -P)
[ "$WORKDIR" != / ] || fail "the working directory is /; nothing to sandbox"
if [ -L "$WORKDIR" ] || [ -e "$WORKDIR.pre-marsh" ]; then
    fail "$WORKDIR was already moved onto marsh storage"
fi

mkdir -p "$STORE"
if [ "$(stat -f -c %T "$STORE")" != btrfs ]; then
    image=/marsh-store.img
    truncate -s "$LOOP_SIZE" "$image"
    btrfs_tool mkfs.btrfs -q "$image" >/dev/null
    mount -o loop,user_subvol_rm_allowed "$image" "$STORE" \
        || fail "cannot loop-mount btrfs; bind-mount a btrfs /marsh-store or run privileged"
fi

trial=$(mktemp -d "$STORE/trial.XXXXXX")
source="$trial/$(basename "$WORKDIR")"
btrfs_tool btrfs -q subvolume create "$source" >/dev/null
cp -a "$WORKDIR/." "$source/"
chown --reference="$WORKDIR" "$source"
chmod --reference="$WORKDIR" "$source"
if [ -n "$AGENT_USER" ]; then
    # Marsh keeps its private state beside the source, so the agent must own the parent.
    chown "$AGENT_USER" "$trial"
fi
mv "$WORKDIR" "$WORKDIR.pre-marsh" || fail "cannot move $WORKDIR (is it a mount point?)"
ln -s "$source" "$WORKDIR"

mkdir -p /logs/agent
# The rmux daemon runs as the agent and opens the log at startup.
touch /logs/agent/marsh-policy.jsonl
if [ -n "$AGENT_USER" ]; then
    chown "$AGENT_USER" /logs/agent/marsh-policy.jsonl
fi
cat >/etc/marsh.env <<EOF
MARSH_AGENT_POLICY=$MODE
MARSH_POLICY_LOG=/logs/agent/marsh-policy.jsonl
EOF
install -m 0755 "$BUNDLE/libexec/tmux" /usr/local/bin/tmux
echo "marsh-setup: $WORKDIR -> $source ($MODE)"
