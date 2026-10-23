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
    package = lib.mkOption {
      type = lib.types.package;
      default = mkCbox {
        inherit pkgs;
        image =
          {inherit (cfg.image) nixpkgs modules;}
          # This system's nixpkgs is a plain path, without the date a flake
          # input carries; the version suffix has it instead.
          // lib.optionalAttrs (cfg.image.nixpkgs == hostNixpkgs) {
            nixpkgsDate = hostNixpkgsDate;
          };
      };
      defaultText = lib.literalExpression "cbox built from programs.cbox.image";
      description = "The cbox package.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
  };
}
