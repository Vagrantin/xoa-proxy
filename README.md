# xoa-proxy

**XOA Deploy Proxy, HTTP(S) + gunzip bridge for XAPI `VM.import`.**

XAPI's `VM.import` speaks plain HTTP and cannot consume gzip-compressed streams or reach HTTPS sources directly. This Rust daemon bridges the gap so XO Lite CE can deploy the community XOA image from any URL:

```
XAPI → GET http://127.0.0.1:9001/image.xva?src=<url>
     → proxy detects format (extension, HEAD-probe fallback)
     → fetches upstream over HTTP or HTTPS
     → decompresses gzip on the fly when needed
     → streams the raw .xva back to XAPI over plain HTTP
```

The XO Lite frontend always builds the same proxy URL regardless of source scheme or compression, format detection lives entirely in the proxy. SSL verification is controlled per request via the `verify_ssl` query parameter (default `true`); verifying and non-verifying clients are both built at startup, so no restart is needed. An import lock serialises concurrent deployments.

## Code layout

```
src/main.rs   , entry point, logging, router assembly, graceful shutdown
src/config.rs , CLI / env-var configuration (clap derive)
src/state.rs  , shared AppState (two HTTP clients + import lock)
src/stream.rs , fetch pipeline, GuardedStream RAII type, ImageFormat enum
src/handler.rs, axum route handlers (/image.xva, fallback)
src/error.rs  , ProxyError → HTTP response mapping
tests/        , integration tests
```

Stack: tokio + axum (server), reqwest with rustls only, no OpenSSL dependency, deliberately, so static/musl builds for XCP-ng dom0 work.

## Build & test

```bash
cargo build --release
cargo test
```

## Packaging

Shipped as an RPM for XCP-ng 8.3 (`SPECS/xoa-proxy.spec`), with systemd unit, preset, and logrotate config under `packaging/`. It is a hard `Requires:` of the `xo-lite-ce` package (see `../xolite-ce`) and is built/released via the CI pipeline driven by `../buildorchestration`. `xcp-ng-ce-public.asc` is the community repo signing key.
