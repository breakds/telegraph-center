{ inputs, ... }:

let
  inherit (inputs)
    self
    nixpkgs
    crane
    advisory-db
    ;
in
{
  perSystem =
    {
      system,
      pkgs-dev,
      lib,
      ...
    }:
    let
      craneLib = crane.mkLib pkgs-dev;

      src = craneLib.cleanCargoSource ../.;
      hasCargoToml = builtins.pathExists ../Cargo.toml;

      commonArgs = {
        inherit src;
        strictDeps = true;
        buildInputs =
          with pkgs-dev;
          [
            openssl.dev
          ]
          ++ lib.optionals pkgs-dev.stdenv.isDarwin [
            pkgs-dev.libiconv
          ];
      };

      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    in
    {
      _module.args.pkgs-dev = import nixpkgs {
        inherit system;
        config.allowUnfree = true;
      };

      checks = lib.optionalAttrs hasCargoToml {
        clippy = craneLib.cargoClippy (commonArgs // { inherit cargoArtifacts; });

        doc = craneLib.cargoDoc (
          commonArgs
          // {
            inherit cargoArtifacts;
            env.RUSTDOCFLAGS = "--deny warnings";
          }
        );

        fmt = craneLib.cargoFmt {
          inherit src;
        };

        toml-fmt = craneLib.taploFmt {
          src = lib.sources.sourceFilesBySuffices src [ ".toml" ];
        };

        audit = craneLib.cargoAudit {
          inherit src advisory-db;
        };

        deny = craneLib.cargoDeny {
          inherit src;
        };

        nextest = craneLib.cargoNextest (
          commonArgs
          // {
            inherit cargoArtifacts;
            partitions = 1;
            partitionType = "count";
            cargoNextestPartitionsExtraArgs = "--no-tests=pass";
          }
        );
      };

      devShells.default = craneLib.devShell {
        checks = self.checks."${system}";
        packages = with pkgs-dev; [
          cargo-deny
          cargo-nextest
          cargo-watch
          pkg-config
          rust-analyzer
          taplo
        ];
      };
    };
}
