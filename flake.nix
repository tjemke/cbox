{
  description = "cbox: ephemeral NixOS microVMs for coding agents, with a headed browser";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    flake-utils,
    microvm,
    ...
  }: let
    mkCbox = {
      pkgs,
      # The guest image, as programs.cbox.image describes it: `nixpkgs`, the
      # tree it is built from (a flake input or a path; default cbox's own
      # pin), and `modules`, extra NixOS modules for it. `nixpkgsDate` is the
      # day that tree was cut (YYYYMMDD, or null if unknown), which `cbox run`
      # warns about once it is old; a flake input brings its own.
      image ? {},
    }: let
      guestNixpkgs = image.nixpkgs or nixpkgs;
      guestNixpkgsDate =
        image.nixpkgsDate
        or (
          if guestNixpkgs ? lastModifiedDate
          then builtins.substring 0 8 guestNixpkgs.lastModifiedDate
          else null
        );
    in
      pkgs.callPackage ./package.nix {
        inherit guestNixpkgsDate;
        guest = import ((guestNixpkgs.outPath or guestNixpkgs) + "/nixos/lib/eval-config.nix") {
          system = null; # from nixpkgs.hostPlatform below
          modules =
            [
              microvm.nixosModules.microvm
              ./guest.nix
              {
                nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;
                # QEMU and the rest of the runner run on the host, where QEMU
                # is the boundary the box rests on, so they come from the
                # host's packages whatever the guest is built from.
                microvm.vmHostPackages = pkgs;
              }
            ]
            ++ image.modules or [];
        };
      };
  in
    # KVM guests only: Linux hosts, same-arch guests.
    flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux"] (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      packages = {
        default = mkCbox {inherit pkgs;};
        rtk-oss = pkgs.callPackage ./rtk-oss.nix {};
      };
      formatter = pkgs.alejandra;
    })
    // {
      lib = {inherit mkCbox;};
      nixosModules.default = import ./module.nix {inherit mkCbox;};
    };
}
