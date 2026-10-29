{mkCbox}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.cbox;

  # The nixpkgs this system is built from. A flake-built system records it as
  # the store path of nixpkgs' flake; otherwise (channels) all there is is the
  # path its pkgs were imported from.
  hostNixpkgs =
    if config.nixpkgs.flake.source != null
    then config.nixpkgs.flake.source
    else pkgs.path;

  # nixpkgs' flake stamps ".YYYYMMDD.shortrev" into the version suffix. A
  # channel or a plain checkout doesn't, and then there's no date to warn on.
  hostNixpkgsDate = let
    m = builtins.match "[.]([0-9]{8})[.].*" config.system.nixos.versionSuffix;
  in
    if m == null
    then null
    else builtins.head m;
in {
  imports = [
    (lib.mkRemovedOptionModule ["programs" "cbox" "guestNixpkgs"] ''
      It is now programs.cbox.image.nixpkgs, and takes a nixpkgs rather than
      "host" or "pinned": leave it unset for this system's own (was "host"), or
      set it to inputs.cbox.inputs.nixpkgs for cbox's pin (was "pinned").
    '')
    (lib.mkRemovedOptionModule ["programs" "cbox" "guestModules"] ''
      It is now programs.cbox.image.modules.
    '')
  ];

  options.programs.cbox = {
    enable = lib.mkEnableOption "cbox, ephemeral NixOS microVMs for coding agents";
    image = {
      nixpkgs = lib.mkOption {
        type = lib.types.path;
        default = hostNixpkgs;
        defaultText = lib.literalMD "the nixpkgs this system is built from";
        example = lib.literalExpression "inputs.nixpkgs-unstable";
        description = ''
          The nixpkgs the guest image is built from, a flake input or a path to
          a nixpkgs tree: the guest's kernel, Nix daemon, claude-code and the
          rest of the box come from it. The default is this system's own, so
          updating and rebuilding the host rebuilds the image too.
          `inputs.cbox.inputs.nixpkgs` is cbox's own pin. `cbox run` warns once
          it is a month old, when the date is known: a flake input carries
          one, and so does this system's nixpkgs if the system is built from a
          flake. QEMU and everything else cbox runs on the host come from this
          system's packages regardless.
        '';
      };
      modules = lib.mkOption {
        type = lib.types.listOf lib.types.deferredModule;
        default = [];
        example = lib.literalExpression ''
          [
            ({pkgs, ...}: {
              microvm.mem = 12288;
              environment.systemPackages = [pkgs.nodejs];
            })
          ]
        '';
        description = "Extra NixOS modules for the guest image (packages, `microvm.mem`/`vcpu`, `cbox.*`).";
      };
    };
    # Per-project persistence is the guest's cbox.persist.project, set in
    # image.modules: it lives in the shared directory and needs nothing from
    # the host. This is the part the host has to enforce.
    persist.global = {
      settings = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = ["model" "outputStyle" "alwaysThinkingEnabled"];
        description = ''
          Top-level keys of the guest's ~/.claude/settings.json to carry from
          one box to every later one, whatever its project: `model` makes a
          `/model` choice stick. When a box's session ends, cbox reads these
          keys out of it and keeps them in
          `$XDG_STATE_HOME/cbox/persist-global.json`; later boxes start with
          them on top of `cbox.claudeCode.settings`. Only
          string, number and boolean values are kept, so nothing that holds
          hooks, MCP servers, plugins or environment variables can come
          across, and keys whose string is a command (`apiKeyHelper`) are
          refused. Listing any key here or in `claudeJson` builds cbox with
          its `persist-global` feature, which links the cbox-persist-global
          crate (about 250 lines) and serde_json; without it, neither is
          built.
        '';
      };
      claudeJson = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        example = ["theme" "editorMode" "hasSeenAutoModeEntryWarning"];
        description = ''
          The same, for top-level keys of the guest's ~/.claude.json, where
          Claude Code keeps `/config` choices such as `theme` and
          `editorMode`, and which notices have been seen.
        '';
      };
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = mkCbox {
        inherit pkgs;
        inherit (cfg) persist;
        image =
          {inherit (cfg.image) nixpkgs modules;}
          # This system's nixpkgs is a plain path, without the date a flake
          # input carries; the version suffix has it instead.
          // lib.optionalAttrs (cfg.image.nixpkgs == hostNixpkgs) {
            nixpkgsDate = hostNixpkgsDate;
          };
      };
      defaultText = lib.literalExpression "cbox built from programs.cbox.image and programs.cbox.persist";
      description = "The cbox package.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
  };
}
