# rtk without its compression engine: the Apache-2.0 edition upstream ships as
# rtk-oss. Since 1.0.0 the default build links rtk-engine, which is BUSL-1.1
# and so unfree to nixpkgs; `--no-default-features` leaves it out. What goes
# with it (PostToolUse compression of Read and MCP output, `rtk pipeline`,
# pipeline.toml) is nothing cbox wires up: the PreToolUse rewrite, every
# command filter, `rtk init` and `rtk gain` are Apache-2.0 and unchanged.
# Before 1.0.0 there was no engine, and rtk is used as it is.
#
# Kept here until nixpkgs has an rtk-oss of its own.
{
  lib,
  rtk,
}:
if lib.versionOlder rtk.version "1.0.0"
then rtk
else
  rtk.overrideAttrs (old: {
    pname = "rtk-oss";
    # buildRustPackage turns buildNoDefaultFeatures into these before
    # overrideAttrs gets to see it, so set them directly.
    cargoBuildNoDefaultFeatures = true;
    cargoCheckNoDefaultFeatures = true;
    meta = old.meta // {license = lib.licenses.asl20;};
  })
