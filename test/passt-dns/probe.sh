#!/usr/bin/env bash
# Check where passt actually sends the guest's DNS queries -- with no VM, no
# KVM, no root and no tap device.
#
# passt's --socket mode speaks qemu's `-netdev stream` framing (a 4-byte
# big-endian length, then one raw Ethernet frame), and nothing about that
# requires a virtual machine on the other end. fakeguest.js is that other end:
# it sends one real DNS query to 169.254.1.1:53, the address cbox's guest has as
# its only nameserver, and prints whatever comes back. passt's own --debug flow
# log says where it decided to forward to, which is the thing actually under
# test.
#
#   ./probe.sh            compare cbox's current passt flags against the broken
#                         ones that caused `Unable to connect to API (ENOTIMP)`
#   ./probe.sh repro      additionally reproduce the ENOTIMP end to end, by
#                         putting a DNS server that answers RCODE 4 on
#                         127.0.0.1:53 inside a throwaway network namespace
#
# Needs: passt and node on PATH, and for `repro`, unprivileged user namespaces.
# See NOTES.md §1.
set -uo pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
WORK=${TMPDIR:-/tmp}/passt-dns-probe.$$
mkdir -p "$WORK"
trap 'rm -rf -- "$WORK"' EXIT

QNAME=${QNAME:-api.anthropic.com}

have() { command -v "$1" >/dev/null 2>&1; }
have passt || { echo "probe: no passt on PATH (nix shell nixpkgs#passt)" >&2; exit 1; }
have node || { echo "probe: no node on PATH" >&2; exit 1; }

# case LABEL FLAGS...: run passt with FLAGS, push one DNS query through it.
case_run() {
	local label=$1 dir
	shift
	dir="$WORK/$label"
	mkdir -p "$dir"

	echo "=============================================================="
	echo "CASE $label"
	echo "     passt $*"
	echo "--------------------------------------------------------------"

	# -a/-n/-g are given so the probe does not depend on the host's own
	# addressing; --runas 0:0 because passt would otherwise try to drop to
	# nobody, which fails in a user namespace that maps one uid.
	passt --foreground --debug --one-off --socket "$dir/passt.sock" \
		--runas 0:0 -a 192.0.2.2 -n 24 -g 192.0.2.1 -m 1500 \
		"$@" >"$dir/passt.log" 2>&1 &
	local pid=$! i=0
	until [ -S "$dir/passt.sock" ]; do
		i=$((i + 1))
		kill -0 "$pid" 2>/dev/null || { echo "  !! passt exited:"; sed 's/^/     /' "$dir/passt.log"; return 1; }
		[ "$i" -lt 100 ] || { echo "  !! passt never created its socket"; kill "$pid" 2>/dev/null; return 1; }
		sleep 0.05
	done

	node "$HERE/fakeguest.js" "$dir/passt.sock" "$QNAME"
	sleep 0.3
	kill "$pid" 2>/dev/null
	wait "$pid" 2>/dev/null # this pid only: a bare `wait` would also wait for
	                        # the notimp-server in the repro path, forever

	echo "  --- where passt sent it ---"
	grep -E '=> HOST' "$dir/passt.log" | tail -2 | sed 's/^/     /'
	echo
}

# The two flag sets, as cbox had them and as cbox has them now.
BROKEN=(--ipv4-only --no-map-gw --dns-forward 169.254.1.1 --dns 169.254.1.1)
FIXED=(--ipv4-only --no-map-gw --dns-forward 169.254.1.1 --search none)

if [ "${1:-}" = "_in_netns" ]; then
	# Re-exec target: lo has to be up before anything can bind 127.0.0.1.
	ip link set lo up
	node "$HERE/notimp-server.js" 127.0.0.1 53 &
	srv=$!
	sleep 0.5
	case_run broken-flags-with-a-listener-on-host-loopback "${BROKEN[@]}"
	kill "$srv" 2>/dev/null
	exit 0
fi

case_run broken-flags "${BROKEN[@]}"
case_run current-flags "${FIXED[@]}"

if [ "${1:-}" = repro ]; then
	echo "### Reproducing the ENOTIMP the guest saw."
	echo "### A netns owned by our own user namespace is the only way to bind :53"
	echo "### unprivileged; the server there stands in for whatever answers on the"
	echo "### real host's 127.0.0.1:53."
	echo
	have ip || { echo "probe: repro needs ip(8) (nix shell nixpkgs#iproute2)" >&2; exit 1; }
	exec unshare --user --map-root-user --mount --net --propagation private \
		"$0" _in_netns
fi

cat <<'EOF'
Expected: `broken-flags` forwards to [0.0.0.0]:53, which the kernel routes to
the host's own 127.0.0.1:53, and the guest gets nothing (or whatever is
listening there). `current-flags` forwards to the host's first resolv.conf
nameserver and the guest gets a real answer.

Run `./probe.sh repro` to see the ENOTIMP itself.
EOF
