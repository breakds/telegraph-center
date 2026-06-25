# Reusable NixOS module for Telegraph Center.
#
# It runs the service as a dedicated system user under systemd with a persistent
# state directory for the SQLite database and audio blobs. Secrets are supplied
# through a systemd `EnvironmentFile` (e.g. a ragenix-generated file) and never
# enter the Nix store. The config file (which only names env vars, never secret
# values) is supplied as a path.
#
# Octavian-specific domain, port, secret paths, and the nginx vhost belong in the
# deployment repo or in docs/deployment/octavian-example.nix, not here.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.telegraph-center;
in
{
  options.services.telegraph-center = {
    enable = lib.mkEnableOption "Telegraph Center recording intake and dispatch service";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The telegraph-center package to run (typically the flake's packages.default).";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "telegraph-center";
      description = "System user the service runs as. Owns the data directory.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "telegraph-center";
      description = "System group the service runs as.";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/telegraph-center";
      description = ''
        Persistent directory for the SQLite database and audio blobs. Must match
        the `[data] dir` value in the config file.
      '';
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      description = ''
        Path to the TOML config file. It only names environment variables that
        hold secrets; it never contains secret values.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Optional systemd EnvironmentFile (e.g. ragenix-generated) supplying
        SONIOX_API_KEY, TELEGRAPH_OPERATOR_PASSWORD_HASH, and one variable per
        Webhook Sink secret. Kept out of the Nix store.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    users.users = lib.mkIf (cfg.user == "telegraph-center") {
      telegraph-center = {
        isSystemUser = true;
        group = cfg.group;
        home = cfg.dataDir;
        description = "Telegraph Center service user";
      };
    };

    users.groups = lib.mkIf (cfg.group == "telegraph-center") {
      telegraph-center = { };
    };

    # Ensure the data directory exists with the right ownership regardless of
    # where dataDir points (StateDirectory would only cover the default path).
    systemd.tmpfiles.rules = [
      "d '${cfg.dataDir}' 0750 ${cfg.user} ${cfg.group} - -"
    ];

    systemd.services.telegraph-center = {
      description = "Telegraph Center recording intake and dispatch service";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        TELEGRAPH_CENTER_CONFIG = toString cfg.configFile;
      };

      serviceConfig = {
        ExecStart = "${cfg.package}/bin/telegraph-center";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.dataDir;
        Restart = "on-failure";
        RestartSec = 5;

        # Hardening. ReadWritePaths keeps the data directory writable under the
        # otherwise read-only system view.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        ReadWritePaths = [ cfg.dataDir ];
      } // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = toString cfg.environmentFile;
      };
    };
  };
}
