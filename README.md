# cbox

A fresh NixOS microVM per session, one host directory mounted at `/work`, a
headed Chrome/Chromium on a virtual display, and Claude Code seeded for auto
mode. Exit the shell and the VM, its disks and its keys are deleted.

## Install

```nix
# your system flake
inputs.cbox.url = "path:/path/to/cbox"; # or wherever you keep it
# ...
imports = [inputs.cbox.nixosModules.default];
programs.cbox = {
  enable = true;
  guestModules = [
    ({pkgs, ...}: {
      microvm.mem = 12288; # default 8192, vcpu 4
      environment.systemPackages = [pkgs.nodejs];
    })
  ];
};
```

The first build makes an erofs image of the guest closure (Chrome and
Chromium included), so it takes a while and a few GB. Later sessions reuse it.

## Use

```sh
claude setup-token                    # once, on the host; keep it in KeePassXC
export CLAUDE_CODE_OAUTH_TOKEN=...    # e.g. keepassxc-cli show -s -a Password db.kdbx claude

cbox ~/src/riftbound-proxy-studio -- claude   # or no `-- cmd` for a shell
cbox view        # second terminal: watch/drive DISPLAY=:99 over VNC
cbox attach      # another shell in the same box
cbox ls | stop ID | gc
```

Inside, `nix develop` works (the guest has a writable store overlay on a
scratch disk), downloaded Linux binaries run via nix-ld, and
`~/.claude/CLAUDE.md` tells the agent all of this.

Options: `--git rw` (let the agent commit, see below), `--lan` (allow private
addresses), `--env NAME` (pass another host variable), `--keep` (keep the
session dir and logs in `$XDG_RUNTIME_DIR/cbox/<id>` for debugging).

## What the agent can reach

Files: only the shared directory, through an unprivileged virtiofsd in its
namespace sandbox (pivot_root into the directory; `--sandbox=none` would allow
symlink escapes). The guest boots from an image of its own closure, not your
`/nix/store`. cbox refuses to share `$HOME` or any parent of it.

Network: passt with no inbound forwards and `--no-map-gw`, so the host's
loopback services (Syncthing's API on 127.0.0.1:8384, for one) are unreachable.
IPv4 only. Private/LAN ranges are rejected by nftables inside the guest, where
the agent has no root. That part is defence in depth, not a host-enforced
boundary.

DNS is the exception to the LAN rule. Queries to 169.254.1.1 are forwarded by
passt, on the host, to the host's own first resolver — so if that resolver is a
LAN or corporate one, the box resolves internal names and can use DNS as a
side channel even without `--lan`. The guest's nftables sees only the
169.254.1.1 destination and cannot filter on where it lands.

Secrets: the token travels as a QEMU fw_cfg file, never in argv, and ends up
in the guest's environment. The agent can read it. Revoke it if a session
worries you.

## The shared directory is the attack surface

Anything on the host that executes content from the shared directory is a way
out.

- `.git`: hooks and config (`core.fsmonitor` runs on every `git status`) execute
  on the host. Default `--git ro` makes `/work/.git` a read-only mount point
  that can't be written, renamed or replaced. `--git rw` has no guards: they are
  whack-a-mole (`.git/commondir` alone redirects where config is read from), so
  only use it for tasks you'd trust on the host.
- Submodules: their working trees are writable, and host `git status` reads
  their `.git`. cbox warns and suggests `git -c diff.ignoreSubmodules=all status`.
- Anything else you run from the tree afterwards: `nix develop`, npm scripts,
  editor workspace config. Review the diff first.

## Status

Evaluated, not booted: the whole closure instantiates for x86_64 and aarch64
against nixos-unstable, the host module evaluates, and `cbox.sh` passes
shellcheck. No KVM was available to boot it. If the first boot misbehaves,
run with `--keep` and read `console.log`, `virtiofsd.log` and `passt.log`.

`NOTES.md` has the findings from debugging the first real session — a passt DNS
misconfiguration that sent every guest lookup to the host's `127.0.0.1:53` — plus
the passt DNS model and the caveats found along the way. Read it before touching
the `passt` invocation. That one is fixed and the fix is confirmed:
`test/passt-dns/probe.sh` exercises passt's DNS path with a fake guest over its
qemu socket, so it needs neither KVM nor root, and `./probe.sh repro` reproduces
the original `ENOTIMP` end to end.
