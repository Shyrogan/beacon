{inputs, ...}: {
  imports = [inputs.treefmt-nix.flakeModule];

  perSystem = {config, ...}: {
    treefmt.programs.alejandra.enable = true;
    treefmt.programs.rustfmt.enable = true;
    treefmt.programs.rustfmt.edition = "2024";

    formatter = config.treefmt.build.wrapper;
  };
}
