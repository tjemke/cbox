{mkCbox}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.cbox;

  # nixpkgs' flake stamps ".YYYYMMDD.shortrev" into the version suffix. A
  # channel or a plain checkout doesn't, and then there's no date to warn on.
  hostNixpkgsDate = let
    m = builtins.match "[.]([0-9]{8})[.].*" config.system.nixos.versionSuffix;
  in
    if m == null
    then null
    else builtins.head m;
in {
  options.programs.cbox = {
    enable = lib.mkEnableOption "cbox, ephemeral NixOS microVMs for coding agents";
    guestModules = lib.mkOption {
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
      description = "Extra NixOS modules for the guest (packages, `microvm.mem`/`vcpu`, `cbox.*`).";
    };
    guestNixpkgs = lib.mkOption {
      type = lib.types.enum ["host" "pinned"];
      default = "host";
      description = ''
        Which nixpkgs the guest is built from. "host" is this system's own, so
        updating and rebuilding the host rebuilds the box image too: its Nix
        daemon, kernel and claude-code move with everything else. "pinned" is
        cbox's own flake.lock, which only moves when that lock does. Either
        way `cbox run` warns once the guest's nixpkgs is a month old.
      '';
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = mkCbox ({
          inherit pkgs;
          modules = cfg.guestModules;
        }
        // lib.optionalAttrs (cfg.guestNixpkgs == "host") {
          guestNixpkgs = pkgs.path;
          guestNixpkgsDate = hostNixpkgsDate;
        });
      defaultText = lib.literalExpression "cbox built with programs.cbox.guestModules and guestNixpkgs";
      description = "The cbox package.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
  };
}
