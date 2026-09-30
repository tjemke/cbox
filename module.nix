{mkCbox}: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.cbox;
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
    package = lib.mkOption {
      type = lib.types.package;
      default = mkCbox {
        inherit pkgs;
        modules = cfg.guestModules;
      };
      defaultText = lib.literalExpression "cbox built with programs.cbox.guestModules";
      description = "The cbox package.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [cfg.package];
  };
}
