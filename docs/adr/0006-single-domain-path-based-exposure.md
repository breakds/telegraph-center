# Single-Domain Path-Based Exposure

Telegraph Center is exposed through one public hostname with path-based separation: `/api/*` is the Client API protected by nginx mTLS, and `/monitor/*` is the Operator webapp protected by app-managed login. This keeps the service easy to remember while requiring explicit nginx policy per path and monitor cookies scoped to `/monitor` so Operator Sessions are not sent to upload endpoints.
