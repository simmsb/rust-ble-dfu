{
  description = "Build a cargo project without extra checks";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane = {
      url = "github:ipetkov/crane";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-parts.url = "github:hercules-ci/flake-parts";

    devshell.url = "github:numtide/devshell";
  };

  outputs = inputs @ { self, nixpkgs, flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.flake-parts.flakeModules.easyOverlay
        inputs.devshell.flakeModule
      ];
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];
      perSystem = { system, config, pkgs, ... }:
        let

          native-toolchain = inputs.fenix.packages.${system}.complete.withComponents [
            "cargo"
            "clippy"
            "rust-src"
            "rustc"
            "rustfmt"
          ];

          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain native-toolchain;
          my-crate = craneLib.buildPackage {
            src = craneLib.cleanCargoSource (craneLib.path ./.);

            buildInputs = [
              # Add additional build inputs here
            ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
              # Additional darwin specific inputs can be set here
              pkgs.libiconv
              pkgs.darwin.apple_sdk.frameworks.CoreBluetooth
            ];

            # Additional environment variables can be set directly
            # MY_CUSTOM_VAR = "some value";
          };
        in

        rec {
          packages.dfu_ble = my-crate;
          apps.dfu_ble.program = "${my-crate}/bin/ble-dfu";
          apps.default = apps.dfu_ble;

          overlayAttrs = {
            inherit (config.packages) dfu_ble;
          };

          devshells.default = {
            packagesFrom = [ my-crate ];

            packages = [
              inputs.fenix.packages.${system}.rust-analyzer
            ];
          };

        };
    };
}
