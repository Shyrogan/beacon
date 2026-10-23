{inputs, ...}: {
  perSystem = {
    lib,
    pkgs,
    ...
  }: let
    craneLib = inputs.crane.mkLib pkgs;

    commonArgs = {
      # build.rs reads the extracted light tables, which crane's default
      # filter would drop.
      src = lib.fileset.toSource {
        root = ../.;
        fileset = lib.fileset.unions [
          (craneLib.fileset.commonCargoSources ../.)
          ../data
        ];
      };
      strictDeps = true;
    };

    # Every dependency in Cargo.lock, built once and reused until the lockfile
    # changes.
    cargoArtifacts = craneLib.buildDepsOnly commonArgs;
  in {
    packages.deps = cargoArtifacts;

    checks = {
      clippy = craneLib.cargoClippy (commonArgs
        // {
          inherit cargoArtifacts;
          cargoClippyExtraArgs = "--all-targets -- --deny warnings";
        });
      test = craneLib.cargoTest (commonArgs // {inherit cargoArtifacts;});
    };
  };
}
