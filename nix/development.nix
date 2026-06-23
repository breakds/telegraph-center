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

      # Keep the usual Cargo sources, plus SQL migrations: sqlx::migrate!()
      # embeds them at compile time, so they must be present in the build src.
      src = lib.cleanSourceWith {
        src = ../.;
        name = "source";
        filter =
          path: type:
          (lib.hasSuffix ".sql" path) || (craneLib.filterCargoSources path type);
      };
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
          # sqlx declares sqlx-mysql and sqlx-postgres as optional dependencies,
          # so rsa is recorded in Cargo.lock even though only the SQLite driver
          # is enabled. cargo-audit scans the whole lockfile, so it flags the rsa
          # Marvin attack (RUSTSEC-2023-0071); rsa is never compiled or linked
          # here. --ignore yanked avoids offline crates.io index lookups.
          cargoAuditExtraArgs = "--ignore yanked --ignore RUSTSEC-2023-0071";
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
