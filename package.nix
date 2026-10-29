{
  lib,
  rustPlatform,
  makeWrapper,
  bash,
  openssh,
  passt,
  socat,
  tigervnc,
  virtiofsd,
  guest,
  guestNixpkgsDate ? null,
  # Top-level keys of the guest's ~/.claude/settings.json (`settings`) and
  # ~/.claude.json (`claudeJson`) to carry from one box to every later one, as
  # programs.cbox.persist.global describes. Any at all builds the
  # `persist-global` feature.
  persistGlobal ? {},
}: let
  runner = guest.config.microvm.declaredRunner;

  # Keys whose value is a command Claude Code runs, even as a plain string.
  # Kept across, one box could plant one for every box after it.
  commandKeys = ["apiKeyHelper" "awsAuthRefresh" "awsCredentialExport" "otelHeadersHelper"];
  checkKey = key:
    lib.throwIf (builtins.match "[A-Za-z0-9_.-]+" key == null)
    "cbox: persist.global: not a key name: ${builtins.toJSON key}"
    (lib.throwIf (lib.elem key commandKeys)
      "cbox: persist.global: ${key} holds a command; carried across, one box could set it for every later one"
      key);
  globalKeys = {
    CBOX_PERSIST_GLOBAL_SETTINGS = map checkKey (persistGlobal.settings or []);
    CBOX_PERSIST_GLOBAL_CLAUDE_JSON = map checkKey (persistGlobal.claudeJson or []);
  };
  persistingGlobal = lib.any (keys: keys != []) (lib.attrValues globalKeys);
  globalFlags = lib.concatLists (lib.mapAttrsToList (var: keys:
    lib.optionals (keys != []) ["--set" var (lib.concatStringsSep "," keys)])
  globalKeys);
in
  rustPlatform.buildRustPackage {
    pname = "cbox";
    version = (lib.importTOML ./Cargo.toml).workspace.package.version;

    # Only the Rust sources, so editing guest.nix doesn't rebuild the binary.
    src = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./crates
      ];
    };
    cargoLock.lockFile = ./Cargo.lock;
    # Only what the binary links: cbox-persist-global is built and tested with
    # the feature that pulls it in, and not at all without it.
    buildFeatures = lib.optional persistingGlobal "cbox/persist-global";
    cargoBuildFlags = ["-p" "cbox"];
    cargoTestFlags = ["-p" "cbox"] ++ lib.optionals persistingGlobal ["-p" "cbox-persist-global"];

    nativeBuildInputs = [makeWrapper];
    nativeCheckInputs = [bash]; # the quoting test round-trips through `sh`

    # cbox shells out to all of these; the wrapper is what writeShellApplication's
    # runtimeInputs used to do. Everything coreutils provided (tail, od, tr, cut,
    # realpath, timeout, ...) the binary now does itself.
    postInstall = ''
      wrapProgram $out/bin/cbox \
        --set CBOX_RUNNER ${lib.escapeShellArg runner} \
        ${lib.optionalString (guestNixpkgsDate != null) "--set CBOX_GUEST_NIXPKGS_DATE ${guestNixpkgsDate}"} \
        ${lib.escapeShellArgs globalFlags} \
        --prefix PATH : ${lib.makeBinPath [openssh passt socat tigervnc virtiofsd]}
    '';

    passthru = {inherit guest runner;};
    meta = {
      description = "Ephemeral NixOS microVMs for coding agents, with a headed browser";
      mainProgram = "cbox";
      platforms = lib.platforms.linux;
    };
  }
