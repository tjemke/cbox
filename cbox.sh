# shellcheck shell=bash
# cbox: a fresh NixOS microVM per session, with one host directory at /work.
# CBOX_RUNNER (the microvm.nix runner) is set by package.nix.

usage() {
  cat <<'EOF'
usage:
  cbox [run] [options] [DIR] [-- CMD...]  boot a fresh box with DIR (default: $PWD) at /work;
                                          CMD defaults to a login shell. Exiting it destroys the box.
  cbox attach [ID] [-- CMD...]            another shell in a running box
  cbox view [ID]                          watch/drive the box's display (:99) in a VNC viewer
  cbox ls                                 list running boxes
  cbox stop ID                            shut a box down
  cbox gc                                 remove leftovers of boxes that died uncleanly

run options:
  --git ro|rw   ro (default): /work/.git is read-only in the box, so no commits.
                rw: the agent can commit, and host git will then run whatever it
                wrote into .git (hooks, config, commondir). Trusted tasks only.
  --lan         allow private/LAN destinations (default: blocked inside the guest)
  --env NAME    pass host variable NAME into the box (repeatable).
                CLAUDE_CODE_OAUTH_TOKEN and ANTHROPIC_API_KEY are passed when set.
  --keep        keep the session directory and disks after exit (debugging)

A project directory named like a subcommand needs `cbox run DIR`.
EOF
}

umask 077
shopt -s nullglob

state_root="${XDG_RUNTIME_DIR:?XDG_RUNTIME_DIR is not set}/cbox"
disk_root="${XDG_CACHE_HOME:-$HOME/.cache}/cbox"

die() {
  printf 'cbox: %s\n' "$*" >&2
  exit 1
}

# fail MESSAGE LOG...: show the tail of the given session logs, then exit.
fail() {
  local msg=$1 log
  shift
  for log in "$@"; do
    if [ -s "$RUN/$log" ]; then
      printf '\n--- %s (last 30 lines) ---\n' "$log" >&2
      tail -n 30 "$RUN/$log" >&2
    fi
  done
  die "$msg"
}

# ssh_box SESSION_DIR SSH_ARGS...
ssh_box() {
  local dir=$1 cid
  shift
  cid=$(<"$dir/cid")
  ssh -F /dev/null \
    -i "$dir/client" -o IdentitiesOnly=yes -o IdentityAgent=none \
    -o UserKnownHostsFile="$dir/known_hosts" -o GlobalKnownHostsFile=/dev/null \
    -o HostKeyAlias=cbox -o StrictHostKeyChecking=yes \
    -o ProxyCommand="socat - VSOCK-CONNECT:$cid:22" \
    -o BatchMode=yes -o ConnectTimeout=5 -o ServerAliveInterval=15 -o LogLevel=ERROR \
    "$@"
}

# One shell-quoted string for the guest's login shell to parse.
remote_cmd() {
  local q
  q=$(printf '%q ' /run/current-system/sw/bin/cbox-enter "$@")
  printf '%s' "$q"
}

live_sessions() {
  local d
  for d in "$state_root"/*/; do
    d=${d%/}
    [ -f "$d/qemu.pid" ] || continue
    if kill -0 "$(<"$d/qemu.pid")" 2>/dev/null; then
      printf '%s\n' "$d"
    fi
  done
}

pick_session() {
  local want=${1:-} d
  local -a found=()
  while IFS= read -r d; do
    if [ -z "$want" ] || [ "${d##*/}" = "$want" ]; then
      found+=("$d")
    fi
  done < <(live_sessions)
  case ${#found[@]} in
    1) printf '%s\n' "${found[0]}" ;;
    0) die "no running box${want:+ named $want}" ;;
    *) die "several boxes are running; pass an ID (see: cbox ls)" ;;
  esac
}

wait_for_socket() { # PATH PID NAME LOG
  local _
  for _ in $(seq 1 200); do
    [ -S "$1" ] && return 0
    kill -0 "$2" 2>/dev/null || fail "$3 exited during startup" "$4"
    sleep 0.05
  done
  fail "$3 did not create its socket" "$4"
}

cleanup() {
  trap - EXIT
  if [ -n "${QEMU_PID:-}" ] && kill -0 "$QEMU_PID" 2>/dev/null; then
    (cd "$RUN" && timeout 20 "$CBOX_RUNNER/bin/microvm-shutdown") >/dev/null 2>&1 || true
    kill "$QEMU_PID" 2>/dev/null || true
  fi
  local pid
  for pid in "${FSD_PID:-}" "${NET_PID:-}"; do
    if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  done
  wait 2>/dev/null || true
  if [ "${KEEP:-0}" = 1 ]; then
    printf 'cbox: kept %s and %s\n' "$RUN" "$DISK" >&2
  else
    rm -rf -- "$RUN" "$DISK"
  fi
}

cmd_run() {
  local git_mode=ro lan=0 dir="" name
  local -a pass_env=(CLAUDE_CODE_OAUTH_TOKEN ANTHROPIC_API_KEY) cmd=()
  KEEP=0
  while [ $# -gt 0 ]; do
    case $1 in
      --git)
        [ $# -ge 2 ] || die "--git needs ro or rw"
        git_mode=$2
        shift 2
        ;;
      --git=*)
        git_mode=${1#--git=}
        shift
        ;;
      --lan)
        lan=1
        shift
        ;;
      --env)
        [ $# -ge 2 ] || die "--env needs a variable name"
        pass_env+=("$2")
        shift 2
        ;;
      --keep)
        KEEP=1
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      --)
        shift
        cmd=("$@")
        break
        ;;
      -*) die "unknown option: $1" ;;
      *)
        [ -z "$dir" ] || die "only one DIR can be shared"
        dir=$1
        shift
        ;;
    esac
  done
  case $git_mode in
    ro | rw) ;;
    *) die "--git must be ro or rw" ;;
  esac
  for name in "${pass_env[@]}"; do
    [[ $name =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || die "not a variable name: $name"
  done

  [ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm is not usable by $(id -un)"
  [ -w /dev/vhost-vsock ] || die "/dev/vhost-vsock is not usable by $(id -un)"

  local home
  dir=$(realpath -e -- "${dir:-$PWD}") || die "no such directory"
  [ -d "$dir" ] || die "not a directory: $dir"
  home=$(realpath -e -- "$HOME")
  if [ "$dir" = / ] || [ "$dir" = "$home" ] || [[ $home == "$dir"/* ]]; then
    die "refusing to share $dir: it is \$HOME or contains it"
  fi
  if [ "$git_mode" = rw ]; then
    printf 'cbox: --git rw: review .git before running host git in %s afterwards\n' "$dir" >&2
  elif [ -f "$dir/.gitmodules" ]; then
    printf 'cbox: %s has submodules; their working trees are agent-writable, and host git\n' "$dir" >&2
    printf '      reads their .git on "git status". Afterwards use: git -c diff.ignoreSubmodules=all status\n' >&2
  fi

  local base id rnd
  base=$(printf '%s' "${dir##*/}" | tr -c 'A-Za-z0-9._-' '_' | cut -c1-24)
  rnd=$(od -An -N3 -tx1 /dev/urandom | tr -d ' \n')
  id="$base-$rnd"
  RUN="$state_root/$id"
  DISK="$disk_root/$id"
  mkdir -p "$state_root" "$disk_root"
  mkdir "$RUN" "$DISK" "$RUN/cred"
  trap cleanup EXIT
  trap 'exit 130' INT TERM HUP
  ln -s "$DISK" "$RUN/disk"
  printf '%s\n' "$dir" >"$RUN/project"
  printf '%s\n' $(($(od -An -N4 -tu4 /dev/urandom) % 4000000000 + 1000)) >"$RUN/cid"

  # Per-session keys: the client key logs in, the host key is pinned.
  ssh-keygen -q -t ed25519 -N '' -C cbox -f "$RUN/client"
  ssh-keygen -q -t ed25519 -N '' -C cbox-host -f "$RUN/cred/hostkey"
  printf 'cbox %s\n' "$(<"$RUN/cred/hostkey.pub")" >"$RUN/known_hosts"
  rm "$RUN/cred/hostkey.pub"
  cp "$RUN/client.pub" "$RUN/cred/authorized_keys"
  printf '%s\n' "$git_mode" >"$RUN/cred/git"
  printf '%s\n' "$lan" >"$RUN/cred/lan"
  {
    printf '# cbox session environment\n'
    for name in "${pass_env[@]}"; do
      if [ -n "${!name:-}" ]; then
        printf 'export %s=%q\n' "$name" "${!name}"
      fi
    done
  } >"$RUN/cred/env"

  # Guest writes land as us; our files appear as agent (1000:100) inside.
  # Default --sandbox=namespace: virtiofsd pivot_roots into $dir, which is
  # what stops symlink escapes (--sandbox=none would not).
  local uid gid
  uid=$(id -u)
  gid=$(id -g)
  virtiofsd --socket-path="$RUN/work.sock" --shared-dir="$dir" \
    --sandbox=namespace --cache=auto \
    --translate-uid="squash-guest:0:$uid:4294967295" --translate-uid="host:$uid:1000:1" \
    --translate-gid="squash-guest:0:$gid:4294967295" --translate-gid="host:$gid:100:1" \
    >"$RUN/virtiofsd.log" 2>&1 &
  FSD_PID=$!

  # No inbound forwards (passt's default), no host-loopback mapping, IPv4 only.
  # --dns-forward makes 169.254.1.1 (the guest's only nameserver) the address
  # passt matches DNS on; the resolver it forwards *to* is --dns-host, which
  # defaults to the first nameserver in the host's /etc/resolv.conf. Do not add
  # `--dns ADDR` here: it sets ip4.dns[0], which makes passt's get_dns() skip
  # every `nameserver` line, leaving --dns-host unset (0.0.0.0) and silently
  # NATing all guest DNS to the host's own 127.0.0.1:53. See NOTES.md.
  # --search none keeps the host's internal search domains out of the box.
  # Not --quiet: passt's startup config goes to passt.log, which is cheap and
  # the only record of what the box's network actually looked like.
  passt --foreground --one-off --ipv4-only --socket "$RUN/passt.sock" \
    --no-map-gw --dns-forward 169.254.1.1 --search none \
    >"$RUN/passt.log" 2>&1 &
  NET_PID=$!

  wait_for_socket "$RUN/work.sock" "$FSD_PID" virtiofsd virtiofsd.log
  wait_for_socket "$RUN/passt.sock" "$NET_PID" passt passt.log

  (cd "$RUN" && exec "$CBOX_RUNNER/bin/microvm-run") </dev/null >"$RUN/console.log" 2>&1 &
  QEMU_PID=$!
  printf '%s\n' "$QEMU_PID" >"$RUN/qemu.pid"

  printf 'cbox: booting %s\n' "$id" >&2
  local deadline=$((SECONDS + 180))
  until ssh_box "$RUN" agent@cbox -- test -e /run/cbox/ready 2>/dev/null; do
    kill -0 "$QEMU_PID" 2>/dev/null || fail "the VM exited during boot" console.log virtiofsd.log
    [ "$SECONDS" -lt "$deadline" ] || fail "the VM was not ready after 180s" console.log
    sleep 1
  done
  rm -rf -- "$RUN/cred" # QEMU read the fw_cfg files at startup

  local rc=0
  ssh_box "$RUN" -t agent@cbox -- "$(remote_cmd "${cmd[@]}")" || rc=$?
  exit "$rc"
}

cmd_attach() {
  local id="" dir
  local -a cmd=()
  while [ $# -gt 0 ]; do
    case $1 in
      --)
        shift
        cmd=("$@")
        break
        ;;
      -*) die "unknown option: $1" ;;
      *)
        id=$1
        shift
        ;;
    esac
  done
  dir=$(pick_session "$id")
  ssh_box "$dir" -t agent@cbox -- "$(remote_cmd "${cmd[@]}")"
}

cmd_view() {
  local dir port tries=0 rc=0
  dir=$(pick_session "${1:-}")
  until ssh_box "$dir" -f -N -M -S "$dir/vnc.ctl" -o ExitOnForwardFailure=yes \
    -L "127.0.0.1:${port:=$(($(od -An -N2 -tu2 /dev/urandom) % 20000 + 40000))}:127.0.0.1:5900" agent@cbox; do
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || die "could not set up the VNC tunnel"
    unset port
  done
  vncviewer "127.0.0.1::$port" || rc=$?
  ssh -F /dev/null -S "$dir/vnc.ctl" -O exit cbox 2>/dev/null || true
  return "$rc"
}

cmd_ls() {
  local d
  while IFS= read -r d; do
    printf '%s\t%s\n' "${d##*/}" "$(<"$d/project")"
  done < <(live_sessions)
}

cmd_stop() {
  local dir
  [ $# -eq 1 ] || die "usage: cbox stop ID"
  dir=$(pick_session "$1")
  (cd "$dir" && "$CBOX_RUNNER/bin/microvm-shutdown")
}

cmd_gc() {
  local d
  for d in "$state_root"/*/; do
    d=${d%/}
    [ -f "$d/qemu.pid" ] || continue
    kill -0 "$(<"$d/qemu.pid")" 2>/dev/null && continue
    printf 'cbox: removing %s\n' "${d##*/}" >&2
    rm -rf -- "${disk_root:?}/${d##*/}" "$d"
  done
}

case ${1:-} in
  run)
    shift
    cmd_run "$@"
    ;;
  attach)
    shift
    cmd_attach "$@"
    ;;
  view)
    shift
    cmd_view "$@"
    ;;
  ls) cmd_ls ;;
  stop)
    shift
    cmd_stop "$@"
    ;;
  gc) cmd_gc ;;
  help | -h | --help) usage ;;
  *) cmd_run "$@" ;;
esac
