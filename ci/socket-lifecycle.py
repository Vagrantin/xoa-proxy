#!/usr/bin/env python3
"""CI-only real systemd lifecycle checks and matched synthetic CPU/RSS samples."""
import gzip
import http.client
import http.server
import json
import os
from pathlib import Path
import random
import re
import shutil
import socket
import subprocess
import sys
import threading
import time
import urllib.parse

baseline, candidate, output = map(Path, sys.argv[1:])
output.mkdir(parents=True, exist_ok=True)
payload = random.Random(64).randbytes(16 * 1024 * 1024)
compressed = gzip.compress(payload, compresslevel=1)


class Upstream(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        data = compressed if self.path.endswith('.gz') else payload
        self.send_response(200)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        try:
            for offset in range(0, len(data), 65536):
                self.wfile.write(data[offset:offset + 65536])
                self.wfile.flush()
                time.sleep(0.012)  # imports outlast the 2-second CI idle window
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *args):
        pass


server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
threading.Thread(target=server.serve_forever, daemon=True).start()


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def ctl(*args, check=True):
    return subprocess.run(['systemctl', *args], check=check, text=True, capture_output=True).stdout.strip()


def wait_for(predicate, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise AssertionError('Timed out waiting for lifecycle transition')


def stats(pid):
    if not pid:
        return {'rss_kib': 0, 'cpu_seconds': 0, 'threads': 0}
    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
    status = Path(f'/proc/{pid}/status').read_text().splitlines()
    rss = next(int(line.split()[1]) for line in status if line.startswith('VmRSS:'))
    return {'rss_kib': rss, 'cpu_seconds': (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK'), 'threads': int(fields[17])}


def measure(pid, action):
    start = stats(pid)
    samples = [start['rss_kib']]
    stop = threading.Event()

    def sample():
        while not stop.wait(0.05):
            samples.append(stats(pid)['rss_kib'])

    sampler = threading.Thread(target=sample)
    sampler.start()
    began = time.monotonic()
    try:
        action()
    finally:
        wall = time.monotonic() - began
        stop.set()
        sampler.join()
    end = stats(pid)
    cpu = end['cpu_seconds'] - start['cpu_seconds']
    return {'seconds': round(wall, 3), 'cpu_seconds': cpu,
            'cpu_percent_one_core': round(cpu / wall * 100, 2),
            'peak_sampled_rss_kib': max(samples + [end['rss_kib']]), 'threads': end['threads']}


def request(port, path, cancel=False):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=15)
    try:
        conn.request('GET', path)
        response = conn.getresponse()
        if cancel:
            assert response.status == 200
            response.read(65536)
            return
        data = response.read()
        if path.startswith('/image.xva?'):
            assert response.status == 200 and data == payload, (response.status, len(data))
            assert response.version == 10  # XAPI must never see chunk framing
        else:
            assert response.status == 400
    finally:
        conn.close()


def import_path(suffix):
    src = f'http://127.0.0.1:{server.server_port}/image.xva{suffix}'
    return '/image.xva?' + urllib.parse.urlencode({'src': src})


def old_ready(port):
    try:
        request(port, '/image.xva')
        return True
    except ConnectionRefusedError:
        return False


report = {}
unit = 'xoa-proxy-ci'
unit_dir = Path('/run/systemd/system')
process = None
service_binary = Path('/opt/xoa-proxy-ci')
old_binary = Path('/opt/xoa-proxy-ci-baseline')
try:
    port = free_port()
    env = dict(os.environ, XOA_PROXY_PORT=str(port), XOA_PROXY_IDLE_SECONDS='2')
    process = subprocess.Popen([str(baseline.resolve())], env=env)
    # Avoid consuming the baseline's startup time in the idle sample.
    def ready():
        assert process.poll() is None
        try:
            request(port, '/image.xva')
            return True
        except ConnectionRefusedError:
            return False
    wait_for(ready)
    report['baseline'] = {'idle': measure(process.pid, lambda: time.sleep(2))}
    for kind, suffix in [('raw', ''), ('gzip', '.gz')]:
        report['baseline'][kind] = measure(process.pid, lambda: request(port, import_path(suffix)))
    process.terminate()
    process.wait(timeout=15)
    process = None

    # Verify the actual shipped units, changing only name, path, port and timeout.
    root = Path(__file__).resolve().parents[1]
    port = free_port()
    # ProtectHome=yes in the shipped unit also hides the runner's workspace.
    shutil.copy2(candidate, service_binary)
    service = (root / 'packaging/xoa-proxy.service').read_text().replace('xoa-proxy.socket', unit + '.socket').replace('/opt/xensource/bin/xoa-proxy', str(service_binary)).replace('XOA_PROXY_IDLE_SECONDS=60', 'XOA_PROXY_IDLE_SECONDS=2')
    sock = (root / 'packaging/xoa-proxy.socket').read_text().replace('127.0.0.1:9001', f'127.0.0.1:{port}')
    (unit_dir / (unit + '.service')).write_text(service)
    (unit_dir / (unit + '.socket')).write_text(sock)
    ctl('daemon-reload')
    ctl('start', unit + '.socket')

    def pid():
        return int(ctl('show', unit + '.service', '--property=MainPID', '--value'))

    assert pid() == 0
    report['candidate'] = {'cold_idle': measure(0, lambda: time.sleep(2))}
    assert pid() == 0
    began = time.monotonic()
    request(port, '/image.xva')
    report['candidate']['activation_seconds'] = round(time.monotonic() - began, 3)
    active_pid = pid()
    assert active_pid > 0
    for kind, suffix in [('raw', ''), ('gzip', '.gz')]:
        report['candidate'][kind] = measure(active_pid, lambda: request(port, import_path(suffix)))
        assert pid() == active_pid, 'Idle timer terminated an active import'
    wait_for(lambda: pid() == 0)
    assert ctl('is-active', unit + '.socket') == 'active'
    assert ctl('show', unit + '.service', '-p', 'Result', '--value') == 'success'
    report['candidate']['settled_idle'] = measure(0, lambda: time.sleep(2))
    request(port, import_path(''))  # next deployment reactivates the service
    assert pid() > 0
    request(port, import_path('.gz'), cancel=True)
    wait_for(lambda: pid() == 0)
    request(port, import_path('.gz'))  # cancellation released the import lock
    wait_for(lambda: pid() == 0)

    # Exercise the actual RPM scriptlets: migrate an old running, boot-enabled
    # daemon while its transfer is still active, then remove the socket package.
    ctl('stop', unit + '.socket', unit + '.service')
    shutil.copy2(baseline, old_binary)
    old_service = (root / 'baseline/packaging/xoa-proxy.service').read_text().replace('/opt/xensource/bin/xoa-proxy', str(old_binary)).replace('[Service]', f'[Service]\nEnvironment=XOA_PROXY_PORT={port}')
    (unit_dir / (unit + '.service')).write_text(old_service)
    ctl('daemon-reload')
    ctl('enable', '--runtime', unit + '.service')
    ctl('start', unit + '.service')
    # Ensure the old listener is ready before beginning the upgrade transfer.
    wait_for(lambda: old_ready(port))
    failures = []
    def transferring():
        try:
            request(port, import_path('.gz'))
        except Exception as error:
            failures.append(error)
    transfer = threading.Thread(target=transferring)
    transfer.start()
    time.sleep(0.5)
    assert transfer.is_alive()
    (unit_dir / (unit + '.service')).write_text(service)

    def scriptlet(spec, name, arg):
        text = spec.read_text()
        body = re.search(r'^%' + name + r'\n(.*?)(?=^%[a-z]|\Z)', text, re.M | re.S).group(1)
        body = body.replace('xoa-proxy.service', unit + '.service').replace('xoa-proxy.socket', unit + '.socket')
        subprocess.run(['sh', '-c', body, 'rpm-scriptlet', str(arg)], check=True)

    scriptlet(root / 'SPECS/xoa-proxy.spec', 'post', 2)
    scriptlet(root / 'baseline/SPECS/xoa-proxy.spec', 'postun', 1)
    transfer.join(timeout=15)
    assert not transfer.is_alive() and not failures, failures
    assert ctl('is-enabled', unit + '.socket') == 'enabled'
    assert ctl('is-enabled', unit + '.service', check=False) == 'static'
    wait_for(lambda: pid() == 0)
    request(port, import_path(''))
    wait_for(lambda: pid() == 0)
    scriptlet(root / 'SPECS/xoa-proxy.spec', 'preun', 0)
    assert ctl('is-active', unit + '.socket', check=False) == 'inactive'
    assert pid() == 0
    scriptlet(root / 'SPECS/xoa-proxy.spec', 'postun', 0)

    # Explicit standalone execution remains always-on despite the idle setting.
    port = free_port()
    env.update(XOA_PROXY_PORT=str(port))
    process = subprocess.Popen([str(candidate.resolve())], env=env)
    wait_for(ready)
    time.sleep(3)
    assert process.poll() is None
    report['candidate']['standalone_idle'] = measure(process.pid, lambda: time.sleep(2))
    process.terminate()
    process.wait(timeout=15)
    process = None
    bad = subprocess.run([str(candidate.resolve())], env=dict(env, LISTEN_PID='invalid', LISTEN_FDS='1'), capture_output=True, timeout=10)
    assert bad.returncode != 0, 'Malformed activation must fail instead of silently binding'
    (output / 'resources.json').write_text(json.dumps(report, indent=2) + '\n')
    rows = ['| Version / phase | RSS peak (KiB, sampled) | CPU (s) | CPU % (one core) | Wall (s) | Threads |', '|---|---:|---:|---:|---:|---:|']
    for version, phases in report.items():
        for phase, result in phases.items():
            if isinstance(result, dict):
                rows.append(f"| {version} / {phase} | {result['peak_sampled_rss_kib']} | {result['cpu_seconds']:.3f} | {result['cpu_percent_one_core']} | {result['seconds']} | {result['threads']} |")
    rows += ['', 'Synthetic 16 MiB raw/gzip transfers with a throttled local upstream. CPU from /proc; RSS sampled at 50 ms. Not real XCP-ng measurements. Socket/kernel/systemd overhead excluded from process figures.', '', 'Lifecycle: cold idle, first activation, long raw/gzip imports, idle exit, reactivation, disconnect recovery, active-import RPM upgrade migration, removal, standalone and malformed activation passed.']
    (output / 'summary.md').write_text('\n'.join(rows) + '\n')
finally:
    (output / 'journal.txt').write_text(subprocess.run(['journalctl', '-u', unit + '.service', '-u', unit + '.socket', '--no-pager'], text=True, capture_output=True).stdout)
    if process is not None:
        process.terminate()
        process.wait(timeout=15)
    ctl('stop', unit + '.socket', unit + '.service', check=False)
    ctl('disable', unit + '.socket', unit + '.service', check=False)
    for ext in ('service', 'socket'):
        (unit_dir / (unit + '.' + ext)).unlink(missing_ok=True)
    ctl('daemon-reload', check=False)
    service_binary.unlink(missing_ok=True)
    old_binary.unlink(missing_ok=True)
    server.shutdown()
