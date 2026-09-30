# The box itself. Everything a session varies (project dir, keys, env, git
# mode, LAN policy, vsock CID) arrives at runtime from cbox.sh; nothing here
# depends on it, so the store image is built once and reused.
{
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.cbox;

  # QEMU fw_cfg entries written by cbox.sh (see microvm.extraArgsScript below).
  fwcfg = "/sys/firmware/qemu_fw_cfg/by_name/opt/cbox";

  cbox-enter = pkgs.writeShellScriptBin "cbox-enter" ''
    set -eu
    if [ -r /run/cbox/env ]; then
      . /run/cbox/env
    fi
    cd /work
    if [ "$#" -eq 0 ]; then
      exec bash -l
    fi
    exec bash -lc 'exec "$@"' cbox-enter "$@"
  '';

  agentNotes = pkgs.writeText "cbox-CLAUDE.md" ''
    # You are inside a cbox

    This is a disposable NixOS microVM. It is destroyed when the session ends;
    only /work (the project directory shared from the host) survives.

    - No root, no sudo. Get tools with `nix shell nixpkgs#<pkg>` or the
      project's `nix develop`. Prebuilt Linux binaries (npm/pip downloads)
      run via nix-ld.
    - A virtual display runs at DISPLAY=:99 (1920x1080). Headed browsers and
      Electron apps work there. Browsers on PATH: `chromium`,
      `google-chrome-stable`. The human may be watching over VNC.
    - Network: public internet only. The host and private/LAN addresses are
      unreachable unless the session was started with --lan.
    - Git: if /work/.git is read-only you cannot commit. Leave your changes in
      the working tree for the human to review and commit.
  '';
in {
  options.cbox = {
    claudeDefaultMode = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = "auto";
      description = ''
        `permissions.defaultMode` seeded into the guest's ~/.claude/settings.json.
        Claude Code only honours "auto" from user settings. null leaves it unset.
      '';
    };
    display = lib.mkOption {
      type = lib.types.str;
      default = "1920x1080x24";
      description = "Xvfb screen geometry for DISPLAY=:99.";
    };
  };

  config = {
    system.stateVersion = lib.trivial.release; # nothing persists, so no migration concerns
    networking.hostName = "cbox";
    nixpkgs.config.allowUnfreePredicate = pkg:
      builtins.elem (lib.getName pkg) [
        "claude-code"
        "google-chrome"
      ];

    microvm = {
      hypervisor = "qemu"; # only runner here with fw_cfg (per-session secrets)
      vcpu = lib.mkDefault 4;
      mem = lib.mkDefault 8192;

      # The VM gets an erofs image of its own closure (storeOnDisk, the default
      # without a /nix/store share), not a view of the host store.
      writableStoreOverlay = "/nix/.rw-store";

      # Relative paths resolve against cbox.sh's per-session directory, where
      # `disk` is a symlink into ~/.cache/cbox/<id>. Sparse; deleted on exit.
      volumes = [
        {
          image = "disk/store.img";
          label = "cbox-store";
          mountPoint = "/nix/.rw-store";
          size = 65536;
          mkfsExtraArgs = ["-q" "-m" "0" "-E" "lazy_itable_init=1,lazy_journal_init=1"];
        }
        {
          image = "disk/home.img";
          label = "cbox-home";
          mountPoint = "/home";
          size = 32768;
          mkfsExtraArgs = ["-q" "-m" "0" "-E" "lazy_itable_init=1,lazy_journal_init=1"];
        }
      ];

      # cbox.sh runs its own unprivileged virtiofsd on this relative socket;
      # `source` is only used by microvm.nix's root virtiofsd launcher, which
      # cbox doesn't use.
      shares = [
        {
          proto = "virtiofs";
          tag = "work";
          source = "/var/empty";
          mountPoint = "/work";
          socket = "work.sock";
        }
      ];

      # passt (started by cbox.sh) instead of QEMU's SLIRP, which maps
      # 10.0.2.2 to the host's loopback services.
      qemu.extraArgs = [
        "-netdev"
        "stream,id=net0,server=off,addr.type=unix,addr.path=passt.sock"
        "-device"
        "virtio-net-pci,netdev=net0,mac=02:c0:de:00:00:01,romfile="
      ];

      # Per-session hypervisor args: the vsock CID and one fw_cfg entry per
      # file in cred/. Files, not strings, so nothing secret lands in argv.
      extraArgsScript = "${pkgs.writeShellScript "cbox-runtime-args" ''
        read -r vsock_cid < cid
        args="-device vhost-vsock-pci,guest-cid=$vsock_cid"
        for f in cred/*; do
          args="$args -fw_cfg name=opt/cbox/''${f##*/},file=$f"
        done
        printf '%s\n' "$args"
      ''}";

      # We bind sshd to vsock ourselves; stop systemd-ssh-generator from
      # adding a second vsock::22 listener.
      kernelParams = ["systemd.ssh_auto=no"];
    };

    fileSystems."/home".neededForBoot = true; # user activation creates ~agent before systemd mounts
    boot.kernelModules = ["qemu_fw_cfg" "vmw_vsock_virtio_transport"];

    users = {
      mutableUsers = false;
      allowNoPasswordLogin = true; # deliberate: no root, no wheel, key-only agent login
      users.root.hashedPassword = "!";
      users.agent = {
        isNormalUser = true;
        uid = 1000; # cbox.sh's virtiofsd shows host-owned files as 1000:100
        group = "users";
      };
    };
    security.sudo.enable = false;

    services.openssh = {
      enable = true;
      startWhenNeeded = true;
      hostKeys = [];
      authorizedKeysInHomedir = false;
      authorizedKeysFiles = ["/run/cbox/authorized_keys"];
      settings = {
        AllowUsers = ["agent"];
        KbdInteractiveAuthentication = false;
        PasswordAuthentication = false;
        PermitRootLogin = "no";
      };
      extraConfig = "HostKey /run/cbox/ssh_host_ed25519_key";
    };
    systemd.sockets.sshd.socketConfig.ListenStream = lib.mkForce "vsock::22";

    networking = {
      useNetworkd = true;
      useDHCP = false;
      enableIPv6 = false;
      nameservers = ["169.254.1.1"]; # passt --dns-forward target
      firewall.enable = true;
      nftables = {
        enable = true;
        # Defence in depth only: passt has no destination filter, so LAN
        # blocking lives in the guest. The agent has no root to undo it.
        # cbox-setup deletes this table for `cbox --lan`.
        tables.cbox-egress = {
          family = "inet";
          content = ''
            chain output {
              type filter hook output priority filter; policy accept;
              oifname "lo" accept
              ip daddr 169.254.1.1 meta l4proto { tcp, udp } th dport { 53, 853 } accept
              udp dport 67 accept
              ip daddr { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4 } reject
              meta nfproto ipv6 reject
            }
          '';
        };
      };
    };
    systemd.network = {
      wait-online.enable = false;
      networks."10-uplink" = {
        matchConfig.Type = "ether";
        networkConfig.DHCP = "ipv4";
        dhcpV4Config.UseDNS = false;
      };
    };

    nix.settings.experimental-features = ["nix-command" "flakes"];
    programs.nix-ld = {
      enable = true;
      libraries = with pkgs; [
        dbus
        expat
        glib
        libx11
        libxcb
        nspr
        nss
        stdenv.cc.cc.lib
        zlib
      ];
    };

    environment = {
      systemPackages = with pkgs; [
        cbox-enter
        chromium
        claude-code
        curl
        fd
        git
        google-chrome
        jaq
        python3
        ripgrep
      ];
      variables.DISPLAY = ":99";
      enableAllTerminfo = true;
    };
    documentation.enable = false;

    systemd.services = {
      cbox-xvfb = {
        description = "Virtual display :99";
        wantedBy = ["multi-user.target"];
        serviceConfig = {
          User = "agent";
          Group = "users";
          ExecStart = "${pkgs.xvfb}/bin/Xvfb :99 -screen 0 ${cfg.display} -nolisten tcp";
          Restart = "always";
        };
      };
      # Guest-local only; `cbox view` reaches it through the ssh tunnel.
      cbox-vnc = {
        description = "VNC view of display :99";
        wantedBy = ["multi-user.target"];
        after = ["cbox-xvfb.service"];
        bindsTo = ["cbox-xvfb.service"];
        serviceConfig = {
          User = "agent";
          Group = "users";
          ExecStart = "${pkgs.tigervnc}/bin/x0vncserver -display :99 -rfbport 5900 -localhost -SecurityTypes None -AlwaysShared";
          Restart = "always";
          RestartSec = 1;
        };
      };

      # Applies the session's settings, then touches /run/cbox/ready. cbox.sh
      # waits for that file before starting the agent, so a failure here
      # fails closed: the session never starts.
      cbox-setup = {
        description = "Apply per-session cbox settings";
        wantedBy = ["multi-user.target"];
        after = ["nftables.service"];
        unitConfig.RequiresMountsFor = ["/work"];
        path = with pkgs; [coreutils kmod nftables util-linux];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
        };
        script = ''
          set -euo pipefail
          modprobe qemu_fw_cfg || true
          cred() { cat "${fwcfg}/$1/raw"; }
          has() { [ -e "${fwcfg}/$1/raw" ]; }

          umask 077
          install -d -m 0755 /run/cbox
          cred hostkey > /run/cbox/ssh_host_ed25519_key
          cred authorized_keys > /run/cbox/authorized_keys
          chmod 0644 /run/cbox/authorized_keys
          if has env; then
            cred env > /run/cbox/env
            chown agent:users /run/cbox/env
            chmod 0400 /run/cbox/env
          fi

          if has lan && [ "$(cred lan)" = 1 ]; then
            nft delete table inet cbox-egress
          fi

          # Host git executes hooks and config from .git (core.fsmonitor on
          # every `git status`). Read-only mode makes .git a read-only mount
          # point: not writable, not renamable, not replaceable.
          mode=ro
          if has git; then mode=$(cred git); fi
          g=/work/.git
          if [ "$mode" = ro ] && { [ -d "$g" ] || [ -f "$g" ]; }; then
            mount --bind "$g" "$g"
            mount -o remount,bind,ro "$g"
          fi

          touch /run/cbox/ready
        '';
      };
    };

    systemd.tmpfiles.settings."10-cbox" = {
      "/tmp/.X11-unix".d = {
        mode = "1777";
        user = "root";
        group = "root";
      };
      "/home/agent/.claude".d = {
        mode = "0700";
        user = "agent";
        group = "users";
      };
      "/home/agent/.claude/CLAUDE.md"."L+".argument = "${agentNotes}";
      "/home/agent/.claude/settings.json".f = lib.mkIf (cfg.claudeDefaultMode != null) {
        mode = "0600";
        user = "agent";
        group = "users";
        argument = builtins.toJSON {permissions.defaultMode = cfg.claudeDefaultMode;};
      };
      # Best effort: skips first-run onboarding and the /work trust prompt.
      "/home/agent/.claude.json".f = {
        mode = "0600";
        user = "agent";
        group = "users";
        argument = builtins.toJSON {
          hasCompletedOnboarding = true;
          projects."/work".hasTrustDialogAccepted = true;
        };
      };
    };
  };
}
