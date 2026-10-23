{
  perSystem = {
    config,
    pkgs,
    ...
  }: {
    devShells.default = pkgs.mkShell {
      packages = [
        pkgs.cargo
        pkgs.clippy
        pkgs.rustc
        pkgs.rust-analyzer
        pkgs.nixd
        config.treefmt.build.wrapper
      ];

      RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
    };
  };
}
