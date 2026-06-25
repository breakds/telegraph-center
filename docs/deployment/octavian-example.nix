# Octavian-shaped deployment example for Telegraph Center.
#
# This mirrors the nixos-machines service-module idiom (a per-service file that
# imports the flake's NixOS module and adds the nginx vhost). It is documentation
# only; copy/adapt it into nixos-machines rather than importing from this repo.
#
# Assumes the flake input is named `telegraph-center` and a service-registry
# entry exists:
#
#   telegraph-center = {
#     domain = "telegraph.breakds.org";
#     port = 7088;
#   };
{
  config,
  lib,
  pkgs,
  inputs,
  ...
}:

let
  registry = (import ../../../data/service-registry.nix).telegraph-center;
in
{
  imports = [ inputs.telegraph-center.nixosModules.telegraph-center ];

  services.telegraph-center = {
    enable = true;
    package = inputs.telegraph-center.packages.${pkgs.system}.default;
    configFile = "/etc/telegraph-center/config.toml";
    # ragenix-generated; never in the Nix store.
    environmentFile = config.age.secrets.telegraph-center-env.path;
    dataDir = "/var/lib/telegraph-center";
  };

  # The TOML config only names env vars (see config.example.toml). Its
  # `[server] listen` must be 127.0.0.1:${registry.port} and `[data] dir` must
  # match the dataDir above.
  environment.etc."telegraph-center/config.toml".source = ./telegraph-center-config.toml;

  services.nginx.virtualHosts."${registry.domain}" = {
    enableACME = true;
    forceSSL = true;

    # Client CA for litewatch mTLS. `optional` keeps /monitor/* reachable from a
    # phone without a Client cert; /api/ enforces a verified cert itself.
    extraConfig = ''
      ssl_client_certificate /etc/telegraph-center/client-ca.pem;
      ssl_verify_client optional;
    '';

    # Client API: mTLS required, 256 MiB cap, trusted fingerprint header.
    locations."/api/" = {
      proxyPass = "http://127.0.0.1:${toString registry.port}";
      # A single proxy_set_header replaces the field for the upstream, so a
      # caller-supplied fingerprint is overwritten and never reaches the app.
      extraConfig = ''
        if ($ssl_client_verify != SUCCESS) { return 403; }
        client_max_body_size 256m;
        proxy_set_header X-Telegraph-Client-Fingerprint "sha1:$ssl_client_fingerprint";
      '';
    };

    # Operator monitor: public TLS, app-managed login, no mTLS, no basic auth.
    locations."/monitor/" = {
      proxyPass = "http://127.0.0.1:${toString registry.port}";
    };
  };
}
