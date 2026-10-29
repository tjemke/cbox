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
  image.modules = [
    ({pkgs, ...}: {
      microvm.mem = 12288; # default 8192, vcpu 4
      environment.systemPackages = [pkgs.nodejs];
    })
  ];
};
```

The first build makes an erofs image of the guest closure (Chrome, Chromium
and Mesa included), so it takes a while and a few GB. Later sessions reuse it.

With the module, the guest image is built from the nixpkgs your system is built
from, so every system update rebuilds it along with it: new Nix daemon, kernel
and claude-code. `programs.cbox.image.nixpkgs` takes any other nixpkgs instead,
such as `inputs.nixpkgs-unstable` on a stable host, or
`inputs.cbox.inputs.nixpkgs` for cbox's own `flake.lock`, which is what
`nix run` of this flake uses. QEMU and the rest of what cbox runs on the host
always come from your system's packages, so they keep up with it whichever
nixpkgs the guest is built from. `cbox run` warns once the guest's nixpkgs is
over 30 days old, since by then it has usually missed fixes for exactly what
stands between the agent and guest root. The date comes from nixpkgs' flake,
the input's own or your system's; a channel-based host has none, and gets no
warning.

claude-code releases faster than nixpkgs follows. To run a newer one without
moving the rest of the box, set `cbox.claudeCode.package` in a guest module,
e.g. to the claude-code of a fresher nixpkgs input.

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
`~/.claude/CLAUDE.md` tells the agent all of this. `nixpkgs#…` in the box is
the nixpkgs the image was built from: its source is in the image (about 200 MB
before compression), so it needs no download and matches what is installed.
`nixpkgs/nixpkgs-unstable#…` still fetches the newest.

[rtk](https://github.com/rtk-ai/rtk) is installed with its Claude Code hook,
so Bash output (git, cargo, test runners, grep, …) is condensed before it
reaches the agent's context. `rtk gain` inside the box shows the savings. The
hook only rewrites commands and never approves them, so auto mode's checks are
unchanged. Turn it off with `cbox.rtk.enable = false;` in a guest module.

Since 1.0, rtk's default build links a compression engine under BUSL-1.1, which
nixpkgs counts as unfree. cbox installs the Apache-2.0 edition upstream ships as
rtk-oss instead: the same rtk without the engine (`rtk-oss.nix`). It keeps the
hook and every command filter, and drops only what cbox doesn't set up:
compressing the output of the agent's Read and MCP tools, and pipelines. For
the full edition, set `cbox.rtk.package = pkgs.rtk;` and
`nixpkgs.config.allowUnfreePackages = ["rtk"];` in a guest module.
cache.nixos.org has neither edition, so rtk is built locally, which takes a few
minutes.

The display has OpenGL, EGL and Vulkan, rendered on the CPU by Mesa's llvmpipe
and lavapipe, so GUIs that need GL start there. Chrome blocklists llvmpipe and
only uses it for WebGL with `--ignore-gpu-blocklist`. There is deliberately no
virtio-gpu/virgl device: it would be faster, but it hands the guest's GL stream
to a renderer on the host. Mesa and its LLVM add about 800 MiB to the image;
`hardware.graphics.enable = false;` in a guest module drops them.

Options: `--git rw` (let the agent commit, see below), `--lan` (allow private
addresses), `--env NAME` (pass another host variable), `--keep` (keep the
session dir and logs in `$XDG_RUNTIME_DIR/cbox/<id>` for debugging).

## Keeping state between boxes

Nothing carries over by default. Everything below is opt-in.

Settings every box starts with, declared in a guest module and merged into the
box's `~/.claude/settings.json` (lists, such as hooks, add up with rtk's):

```nix
image.modules = [{cbox.claudeCode.settings.model = "opus";}];
```

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

Kernel: unprivileged user namespaces are off in the guest, since "root in a
namespace" is where most kernel privilege escalations start. Nix's build
sandbox needs them, so in-box `nix build`s run unsandboxed: impure, but as
`nixbld` users with no more access than the agent. `cbox.nixSandbox = true`
turns both back on. Root in the guest would undo the guest-side protections
(`--git ro`, the LAN block), so this leans towards the smaller kernel
surface.

In the same spirit, the guest's virtualization device nodes (`/dev/vhost-vsock`,
`/dev/vhost-net`, `/dev/kvm`) are locked to root rather than left at systemd's
world-accessible defaults: the box runs under the host's KVM, but nothing the
guest itself runs needs these, and leaving them open would hand the agent more
guest-kernel surface for no gain. Set `cbox.kvm = true;` in a guest module to
give the agent `/dev/kvm` back, for running VM tests or emulators inside the box
where the host allows nested virtualization.

## The shared directory is the attack surface

Anything on the host that executes content from the shared directory is a way
out.

- `.git`: hooks and config (`core.fsmonitor` runs on every `git status`) execute
  on the host. Default `--git ro` makes `/work/.git` a read-only mount point
  that can't be written, renamed or replaced. `--git rw` has no guards: they are
  whack-a-mole (`.git/commondir` alone redirects where config is read from), so
  only use it for tasks you'd trust on the host. cbox says so and asks before
  booting; `--yes` skips the question, and without a terminal it is required.
- Commit plans: with `--git ro`, an agent asked to commit writes
  `commit-plan.sh` instead. It replays the agent's commits onto your index and
  HEAD after checking that they add up to the working tree. It is agent-written
  code for you to run on the host, so read it first; `git status` and
  `git diff` show what the commits will add up to.
- Submodules: their working trees are writable, and host `git status` reads
  their `.git`. cbox warns and suggests `git -c diff.ignoreSubmodules=all status`.
- Anything else you run from the tree afterwards: `nix develop`, npm scripts,
  editor workspace config. Review the diff first.

## Status

Evaluated, not booted: the whole closure instantiates for x86_64 and aarch64
against nixos-unstable, the host module evaluates, and the host CLI (`src/`,
Rust + clap) builds clean under `cargo clippy -- -D warnings` with its unit
tests passing. No KVM was available to boot a real guest. If the first boot
misbehaves, run with `--keep` and read `console.log`, `virtiofsd.log` and
`passt.log`.

`NOTES.md` has the findings from debugging the first real session — a passt DNS
misconfiguration that sent every guest lookup to the host's `127.0.0.1:53` — plus
the passt DNS model and the caveats found along the way. Read it before touching
the `passt` invocation. That one is fixed and the fix is confirmed:
`test/passt-dns/probe.sh` exercises passt's DNS path with a fake guest over its
qemu socket, so it needs neither KVM nor root, and `./probe.sh repro` reproduces
the original `ENOTIMP` end to end.
