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

      # Keep the usual Cargo sources, plus SQL migrations and Askama templates:
      # sqlx::migrate!() and askama both embed these at compile time, so they
      # must be present in the build src.
      src = lib.cleanSourceWith {
        src = ../.;
        name = "source";
        filter =
          path: type:
          (lib.hasSuffix ".sql" path)
          || (lib.hasSuffix ".html" path)
          || (craneLib.filterCargoSources path type);
      };
      hasCargoToml = builtins.pathExists ../Cargo.toml;

      commonArgs = {
        inherit src;
        strictDeps = true;
        # reqwest uses native-tls, so openssl-sys needs pkg-config (a build-time
        # tool) to locate the openssl libraries at compile time.
        nativeBuildInputs = with pkgs-dev; [
          pkg-config
        ];
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

      telegraph-center = craneLib.buildPackage (commonArgs // { inherit cargoArtifacts; });

      # Evaluate the NixOS module in a minimal system so `nix flake check`
      # exercises it: assert the generated unit runs the package, reads the
      # config path, and loads the secret environment file.
      nixosEval = nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          ./nixos-module.nix
          (
            { ... }:
            {
              boot.loader.grub.devices = [ "nodev" ];
              fileSystems."/" = {
                device = "none";
                fsType = "tmpfs";
              };
              system.stateVersion = "24.05";
              services.telegraph-center = {
                enable = true;
                package = telegraph-center;
                configFile = "/etc/telegraph-center/config.toml";
                environmentFile = "/run/secrets/telegraph.env";
              };
            }
          )
        ];
      };
    in
    {
      _module.args.pkgs-dev = import nixpkgs {
        inherit system;
        config.allowUnfree = true;
      };

      packages = lib.optionalAttrs hasCargoToml {
        inherit telegraph-center;
        default = telegraph-center;
      };

      checks = lib.optionalAttrs hasCargoToml {
        clippy = craneLib.cargoClippy (commonArgs // { inherit cargoArtifacts; });

        nixos-module = pkgs-dev.runCommand "telegraph-center-nixos-eval" {
          unit = nixosEval.config.systemd.units."telegraph-center.service".text;
        } ''
          echo "$unit" > unit.txt
          grep -q "/bin/telegraph-center" unit.txt
          grep -q "EnvironmentFile=/run/secrets/telegraph.env" unit.txt
          grep -q "TELEGRAPH_CENTER_CONFIG=/etc/telegraph-center/config.toml" unit.txt
          touch $out
        '';

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
