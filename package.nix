{
  lib,
  writeShellApplication,
  coreutils,
  openssh,
  passt,
  socat,
  tigervnc,
  virtiofsd,
  guest,
}: let
  runner = guest.config.microvm.declaredRunner;
in
  writeShellApplication {
    name = "cbox";
    runtimeInputs = [
      coreutils
      openssh
      passt
      socat
      tigervnc # vncviewer, for `cbox view`
      virtiofsd
    ];
    runtimeEnv.CBOX_RUNNER = "${runner}";
    text = builtins.readFile ./cbox.sh;
    passthru = {inherit guest runner;};
    meta = {
      description = "Ephemeral NixOS microVMs for coding agents, with a headed browser";
      mainProgram = "cbox";
      platforms = lib.platforms.linux;
    };
  }
