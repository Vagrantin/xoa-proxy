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

## RPM repository (GitHub Pages)

Every release is republished as a signed, `yum`-resolvable repository hosted on
GitHub Pages at <https://vagrantin.github.io/xoa-proxy/>, so an installed XCP-HL host
can `yum update xoa-proxy` in place instead of reinstalling from the ISO.

### Recommended: install the whole repository set at once

The instructions below configure this one repository. On an XCP-HL host the
simpler route is the `xcp-hl-release` package, which owns
`/etc/yum.repos.d/xcp-hl.repo` and defines all three XCP-HL repositories
together, so repository configuration arrives through `yum` like any other
update instead of having to be re-downloaded by hand:

```bash
curl -L -o /etc/yum.repos.d/xcp-hl.repo \
  https://vagrantin.github.io/xcp-hl/xcp-hl.repo
rpm --import https://vagrantin.github.io/xcp-hl/xcp-ng-ce-public.asc
yum clean all && yum install xcp-hl-release
```

Hosts installed from a recent ISO already have it. See the
[Updates documentation](https://vagrantin.github.io/xcp-hl/updates.html).

### How it is built

`.github/workflows/pages-repo.yml` builds and deploys the site. It is triggered by
`workflow_run` once *Build and Sign XOA-proxy RPM* completes successfully.

The workflow downloads the `xoa-proxy-*.rpm` assets from the five most recent
releases, indexes them with `createrepo_c`, and signs `repodata/repomd.xml` with
a detached armored signature. The `createrepo_c` flags are necessary for
dom0 on XCP-ng 8.3 is CentOS 7 (yum 3.4.3, rpm 4.11.3), which predates zstd and
zchunk metadata and expects sqlite databases, so `--database
--compress-type=gz --checksum=sha256` are all required for the metadata to be
readable at all.

Only the five most recent releases are published, which bounds the site size and
leaves a rollback window:

```bash
yum --showduplicates list xoa-proxy
yum downgrade xoa-proxy-<version>
```

`pages/` holds the files copied to the site root: `index.html` (the landing page)
and `xcp-hl-xoa-proxy.repo` (ready-made client config), published alongside
`xcp-ng-ce-public.asc`.

### Verification

The client config sets `repo_gpgcheck=1` and `gpgcheck=0`. That asymmetry is
deliberate: the RPMs are signed by a GPG *signing subkey*, and rpm 4.11 registers
only the primary key on import, so it reports `NOKEY` for any subkey-made
signature. Integrity therefore comes from the signed `repomd.xml`, which records
a SHA-256 of `primary.xml`, which records a SHA-256 of every package. This is the
same trust model apt uses, where the release file is signed and the individual
packages are not.

The signing subkeys expire **2027-05-10**. After that date verification fails
until they are extended and `xcp-ng-ce-public.asc` is refreshed here and
re-imported on every host.

## Project entry point

The entry point for the project is the [XCP-HL documentation website](https://vagrantin.github.io/xcp-hl/), and issues must be created on the [xcp-hl repository](https://github.com/Vagrantin/xcp-hl/issues) rather than on this one.
