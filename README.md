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
src/config.rs , environment configuration
src/lifecycle.rs, systemd socket activation and idle lifetime tracking
src/state.rs  , shared AppState (two HTTP clients + import lock)
src/stream.rs , fetch pipeline, GuardedStream RAII type, ImageFormat enum
src/handler.rs, axum route handlers (/image.xva, fallback)
src/error.rs  , ProxyError → HTTP response mapping
tests/        , integration tests
```

Stack: tokio + axum (server), reqwest with rustls only, no OpenSSL dependency, deliberately, so static/musl builds for XCP-ng dom0 work.

## Build & test

Builds run in GitHub Actions: `.github/workflows/xoa-proxy.yml` produces the
static musl RPM; `.github/workflows/check-rust.yml` runs Rust checks, real systemd
socket lifecycle tests and a before/after CPU/RSS benchmark for PRs. The benchmark
is uploaded as `proxy-resources` and summarized in the workflow run. It uses a
synthetic upstream and is not a measurement of a real XCP-ng import.

## On-demand operation (0.1.3 and later)

The RPM enables and starts `xoa-proxy.socket` on `127.0.0.1:9001`. There is no
proxy process at boot. The first TCP connection starts `xoa-proxy.service` with
systemd's inherited listener; queued connections wait for startup. The endpoint
and XO Lite CE deployment flow stay the same.

The proxy exits successfully after 60 seconds with no active requests. This idle
window starts after the last response body is completed or dropped, including
failed or cancelled imports. HEAD probes and upstream connection setup also count
as active work. Graceful shutdown drains accepted connections; it never applies a
fixed time limit to an active import. The socket remains listening for the next
deployment. A single-thread async runtime avoids allocating workers per host CPU.

`systemctl is-active xoa-proxy.socket` should report `active`. An `inactive`
`xoa-proxy.service` is normal between deployments. To stop deployment access, stop
**both** the socket and service; stopping only the service leaves activation enabled:

```bash
systemctl stop xoa-proxy.socket xoa-proxy.service
systemctl start xoa-proxy.socket
```

Upgrades disable the old service's boot enablement and enable the socket. If the
old service is running, its package restart drains the current import before the
socket takes over. Removal stops and disables both units. Local service overrides
should be reviewed before upgrading this alpha release.

Set `XOA_PROXY_IDLE_SECONDS` in a service drop-in to a positive number of seconds
(default 60). Change the address/port with a socket drop-in (`ListenStream=` to
clear the old value, then `ListenStream=127.0.0.1:<port>`); stop both units before
restarting the socket. In socket mode `XOA_PROXY_BIND` and `XOA_PROXY_PORT` do not
change systemd's listener. Direct execution without activation metadata retains
standalone binding via those variables and stays running until signalled.

### Measuring an installed host

Record the package version, host model, image size/format, network speed and
upstream URL for each run. Before and after an actual XO Lite import, capture:

```bash
rpm -q xoa-proxy
systemctl show xoa-proxy.socket -p ActiveState
systemctl show xoa-proxy.service -p ActiveState -p MainPID
ps -C xoa-proxy -o pid,rss,vsz,nlwp,time,pcpu
```

Sample the same process during import with `pidstat -r -u -p <MainPID> 1` if
sysstat is installed. Compare raw and gzip imports with the previous RPM under
the same conditions. Verify the imported VM boots and that the proxy disappears
60 seconds after completion/cancellation, then repeat deployment to verify
reactivation. No PID means **zero proxy-process RSS/CPU**; systemd and the kernel
listening socket still have a small cost. Actual dom0 measurements remain a
release validation step; CI numbers must not be presented as appliance results.

## Packaging

Shipped as an RPM for XCP-ng 8.3 (`SPECS/xoa-proxy.spec`), with systemd unit, preset, and logrotate config under `packaging/`. It is a hard `Requires:` of the `xo-lite-ce` package (see `../xolite-ce`) and is built/released via the CI pipeline driven by `../buildorchestration`. `xcp-ng-ce-public.asc` is the community repo signing key.

## RPM repository

Every release is republished as a signed, `yum`-resolvable repository at
<https://rpm.xcp-hl.org/xoa-proxy/8.3/x86_64/>, built by
[xcp-hl-rpm](https://github.com/Vagrantin/xcp-hl-rpm) (xcp-hl#190), so an installed XCP-HL host
can `yum update xoa-proxy` in place instead of reinstalling from the ISO. The legacy copy at
<https://vagrantin.github.io/xoa-proxy/> stays online until hosts have moved.

### Recommended: install the whole repository set at once

The instructions below configure this one repository. On an XCP-HL host the
simpler route is the `xcp-hl-release` package, which owns
`/etc/yum.repos.d/xcp-hl.repo` and defines all three XCP-HL repositories
together, so repository configuration arrives through `yum` like any other
update instead of having to be re-downloaded by hand:

Hosts installed from a recent ISO already have it. See the
[Updates documentation](https://vagrantin.github.io/xcp-hl/updates.html).

```bash
curl -L -o /etc/yum.repos.d/xcp-hl.repo \
  https://rpm.xcp-hl.org/xcp-hl.repo
rpm --import https://rpm.xcp-hl.org/xcp-ng-ce-public.asc
yum clean all && yum install xcp-hl-release
yum update xoa-proxy
```

Note that `yum` never re-fetches a `.repo` file once it is installed, so a change
to the repository configuration is applied by updating xcp-hl-release RPM package.

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
