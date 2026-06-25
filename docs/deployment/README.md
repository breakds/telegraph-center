# Telegraph Center Deployment

This directory documents how to run Telegraph Center as a real service behind
nginx, and gives copy-pasteable examples. The authoritative contract is:

- one app process listening on loopback (default `127.0.0.1:7088`);
- one public hostname proxying both surfaces;
- `/api/*` protected by nginx mTLS plus a trusted Client fingerprint header;
- `/monitor/*` protected by the app's own Operator login (no mTLS);
- 256 MiB uploads enforced by both nginx and the app.

See [ADR 0006](../adr/0006-single-domain-path-based-exposure.md) and
[ADR 0007](../adr/0007-client-identity-from-nginx-mtls-fingerprint.md).

## Running the binary

The binary takes the config path from the first argument or the
`TELEGRAPH_CENTER_CONFIG` environment variable:

```sh
telegraph-center /etc/telegraph-center/config.toml
# or
TELEGRAPH_CENTER_CONFIG=/etc/telegraph-center/config.toml telegraph-center
```

On startup it opens the SQLite database and blob directories under `[data] dir`,
reclaims any work a previous process left in flight (see below), binds
`[server] listen`, starts the Transcription, Routing, and Delivery worker loops
in-process, and serves until a shutdown signal. Both `SIGINT` (Ctrl-C) and
`SIGTERM` (how systemd stops the unit) trigger graceful shutdown. Missing or
blank secrets (`SONIOX_API_KEY`, Webhook Sink secrets) are startup errors, so a
misconfigured deployment fails fast instead of running degraded.

Crash recovery: a Transcription or Delivery attempt that was in flight when the
process stopped (SIGKILL, crash, power loss, or a `SIGTERM` past systemd's stop
timeout) would otherwise be stranded, since workers skip rows with an unfinished
attempt. At startup — while the process is the sole writer — the service closes
those abandoned attempts as retryable and reverts a Recording that was claimed
but had no attempt recorded, so the normal retry path picks the work up again.
This means a long, in-flight Soniox poll does not need a large
`TimeoutStopSec`; systemd can stop the unit promptly and the next start recovers.

- Config: [config.example.toml](config.example.toml)
- Secret env file: [telegraph.env.example](telegraph.env.example)

The config never contains secret values; it only names environment variables.
See [ADR 0003](../adr/0003-secrets-come-from-environment.md).

## NixOS module

The flake exports `nixosModules.telegraph-center` (also `nixosModules.default`).
A minimal usage:

```nix
services.telegraph-center = {
  enable = true;
  package = inputs.telegraph-center.packages.${pkgs.system}.default;
  configFile = "/etc/telegraph-center/config.toml";
  environmentFile = "/run/secrets/telegraph.env"; # ragenix-generated
  dataDir = "/var/lib/telegraph-center";
};
```

The module runs the service as a dedicated `telegraph-center` system user with a
persistent `dataDir`, restarts on failure, and starts after the network is
online. The `environmentFile` is loaded by systemd and stays out of the Nix
store. `dataDir` must match `[data] dir` in the config file.

## nginx contract

Both surfaces share one public hostname. The key constraint
([ADR 0006](../adr/0006-single-domain-path-based-exposure.md)) is that `/api/*`
requires a Client certificate while `/monitor/*` must remain reachable from a
phone without one. Use `ssl_verify_client optional` at the server level and
enforce a verified certificate only inside the `/api/` location, so monitor
access is never blocked by mTLS.

```nginx
server {
  listen 443 ssl;
  server_name telegraph.example.org;

  # Public server TLS (ACME), as usual.
  ssl_certificate     /var/lib/acme/telegraph.example.org/fullchain.pem;
  ssl_certificate_key /var/lib/acme/telegraph.example.org/key.pem;

  # Client CA used to validate Client (litewatch) certificates. `optional`
  # means a cert is requested but the handshake still succeeds without one, so
  # /monitor/* stays reachable.
  ssl_client_certificate /etc/telegraph-center/client-ca.pem;
  ssl_verify_client      optional;

  # --- Client API: mTLS required ---------------------------------------
  location /api/ {
    # Reject anyone whose Client cert nginx did not successfully verify.
    if ($ssl_client_verify != SUCCESS) { return 403; }

    client_max_body_size 256m;

    # Set the trusted identity from nginx-verified mTLS data. A single
    # proxy_set_header REPLACES the field for the upstream, so any value the
    # caller sent for this header is overwritten and never reaches the app.
    proxy_set_header X-Telegraph-Client-Fingerprint "sha1:$ssl_client_fingerprint";

    proxy_set_header Host              $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_pass http://127.0.0.1:7088;
  }

  # --- Operator monitor: public TLS, app-managed login -----------------
  location /monitor/ {
    # No mTLS requirement here. No basic auth; the app handles Operator login.
    proxy_set_header Host              $host;
    proxy_set_header X-Forwarded-Proto $scheme;
    proxy_pass http://127.0.0.1:7088;
  }
}
```

### Fingerprint variable and format

nginx's built-in `$ssl_client_fingerprint` is the **SHA-1** fingerprint of the
Client certificate (lowercase hex, no colons). We use it directly and prefix the
header value with `sha1:` so the format is explicit and future-proof. The app
reads the header exactly once (`HeaderMap::get`) and does a constant-string match
against `clients[].certificate_fingerprint`, so the configured value must be
exactly what nginx emits.

Set the header with a **single** `proxy_set_header`. nginx forwards the client's
request headers to the upstream by default, but `proxy_set_header` redefines the
named field, so the one trusted assignment overwrites any caller-supplied
`X-Telegraph-Client-Fingerprint` — the app never sees the client's value. This
satisfies [ADR 0007](../adr/0007-client-identity-from-nginx-mtls-fingerprint.md)
(inbound identity must not reach the app) without a second, ambiguous "clear
first" line. (For reference, an *empty* `proxy_set_header value ""` instead
*suppresses* the field; see the
[nginx docs](https://nginx.org/en/docs/http/ngx_http_proxy_module.html#proxy_set_header).)

Compute a Client's configured fingerprint from its certificate:

```sh
# SHA-1 hex with colons, lowercased and stripped to match $ssl_client_fingerprint:
openssl x509 -in client.crt -noout -fingerprint -sha1 \
  | sed 's/.*=//; s/://g' | tr 'A-Z' 'a-z'
# -> e.g. 1a2b3c...  ; put "sha1:1a2b3c..." in certificate_fingerprint
```

SHA-1 is acceptable here because the fingerprint is not a security primitive on
its own: nginx has already cryptographically verified the Client certificate
against the trusted CA, and the fingerprint only maps the verified cert to a
configured Client name. If a SHA-256 binding is later required, it needs an njs
or Lua snippet (nginx has no native SHA-256 fingerprint variable); the app side
only needs the configured string updated to match.

> octavian note: the global nginx `clientMaxBodySize` is `1000m`, above the
> 256 MiB contract, so still set the explicit per-location `client_max_body_size
> 256m` shown above to keep the `/api/` limit unambiguous.

## Hermes webhook route (for the M9 Sink smoke test)

Telegraph delivers Transcripts to a Webhook Sink by `POST`ing JSON. The first
Sink (`journal`) targets Hermes. Hermes must accept, at the configured URL
(e.g. `http://127.0.0.1:8644/webhooks/journal`):

- **Method/body:** `POST` with a JSON body containing Recording metadata and
  `transcript.text`. No audio is sent.
- **Signature:** `X-Webhook-Signature` is the HMAC-SHA256 of the **raw JSON
  body bytes**, keyed by the shared Sink secret, as lowercase hex. Verify it
  over the exact received bytes before parsing.
- **Idempotency:** `X-Request-ID` is the stable Delivery id; treat repeats with
  the same id as duplicates (deliveries are retried).
- `X-Telegraph-Delivery-Id` carries the same Delivery id for clarity.

Hermes is not implemented in this repo.
