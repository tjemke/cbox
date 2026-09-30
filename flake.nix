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
      modules ? [],
    }:
      pkgs.callPackage ./package.nix {
        guest = nixpkgs.lib.nixosSystem {
          modules =
            [
              microvm.nixosModules.microvm
              ./guest.nix
              {nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform.system;}
            ]
            ++ modules;
        };
      };
  in
    # KVM guests only: Linux hosts, same-arch guests.
    flake-utils.lib.eachSystem ["x86_64-linux" "aarch64-linux"] (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      packages.default = mkCbox {inherit pkgs;};
      formatter = pkgs.alejandra;
    })
    // {
      lib = {inherit mkCbox;};
      nixosModules.default = import ./module.nix {inherit mkCbox;};
    };
}
