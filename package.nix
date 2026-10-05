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
}: let
  runner = guest.config.microvm.declaredRunner;
in
  rustPlatform.buildRustPackage {
    pname = "cbox";
    version = (lib.importTOML ./Cargo.toml).package.version;

    # Only the Rust sources, so editing guest.nix doesn't rebuild the binary.
    src = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./src
      ];
    };
    cargoLock.lockFile = ./Cargo.lock;

    nativeBuildInputs = [makeWrapper];
    nativeCheckInputs = [bash]; # the quoting test round-trips through `sh`

    # cbox shells out to all of these; the wrapper is what writeShellApplication's
    # runtimeInputs used to do. Everything coreutils provided (tail, od, tr, cut,
    # realpath, timeout, ...) the binary now does itself.
    postInstall = ''
      wrapProgram $out/bin/cbox \
        --set CBOX_RUNNER ${lib.escapeShellArg runner} \
        --prefix PATH : ${lib.makeBinPath [openssh passt socat tigervnc virtiofsd]}
    '';

    passthru = {inherit guest runner;};
    meta = {
      description = "Ephemeral NixOS microVMs for coding agents, with a headed browser";
      mainProgram = "cbox";
      platforms = lib.platforms.linux;
    };
  }
