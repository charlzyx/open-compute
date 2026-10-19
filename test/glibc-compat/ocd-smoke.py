"""Exercise one packaged daemon, real Worker and persisted KV in CentOS 8.2."""
import hashlib
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request
import uuid

IMAGE = 'centos@sha256:fd84102fc72960dd1b8da0ee3b4c13e3b0c1d2a085de118bc4c97821cd986e02'
ROOT = Path(__file__).resolve().parents[2]
EVIDENCE = ROOT / '.temp/glibc-ocd/evidence'
BINARY = EVIDENCE / 'ocd-linux-x64-glibc228'


def docker(*args, check=True):
    return subprocess.run(['docker', *args], capture_output=True, text=True,
                          timeout=120, check=check)


def main():
    docker('pull', IMAGE)
    mounts = ['--mount', f'type=bind,source={BINARY},target=/ocd,readonly']
    glibc = docker('run', '--rm', '--network', 'none', IMAGE, 'getconf', 'GNU_LIBC_VERSION').stdout.strip()
    assert glibc == 'glibc 2.28'
    version = docker('run', '--rm', '--network', 'none', *mounts, IMAGE, '/ocd', '--no-update-check', '--version').stdout.strip()
    name = 'ocd-centos82-' + uuid.uuid4().hex
    token = ''
    base = ''

    def request(path, method='GET', body=None, content_type='application/json', host=None):
        headers = {'Authorization': 'Bearer ' + token, 'Content-Type': content_type}
        if host:
            headers['Host'] = host
        data = json.dumps(body).encode() if isinstance(body, dict) else body
        req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
        with urllib.request.urlopen(req, timeout=15) as response:
            payload = response.read()
            assert response.status == 200
        if host or '/values/' in path:
            return payload
        value = json.loads(payload)
        assert value['success']
        return value['result']

    def ready():
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            try:
                with urllib.request.urlopen(base + '/health/ready', timeout=1) as response:
                    if response.status == 200:
                        return
            except (OSError, urllib.error.URLError):
                pass
            if docker('inspect', '--format', '{{.State.Running}}', name).stdout.strip() != 'true':
                raise AssertionError('CentOS daemon exited before readiness')
            time.sleep(0.2)
        raise AssertionError('CentOS daemon readiness deadline expired')

    bootstrap = '''set -eu
umask 077
if [ ! -e /root/.open-compute/ocd.toml ]; then
  mkdir -p /root/.open-compute/keys
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \\n' > /root/.open-compute/keys/admin.token
  printf '[server]\\npublic_bind = "0.0.0.0:8787"\\nadmin_auth = { file = "./keys/admin.token" }\\n' > /root/.open-compute/ocd.toml
fi
exec /ocd --no-update-check run
'''
    try:
        docker('run', '--detach', '--name', name, '--cap-drop=ALL',
               '--security-opt=no-new-privileges', '--publish', '127.0.0.1::8787',
               *mounts, IMAGE, 'sh', '-c', bootstrap)
        ports = json.loads(docker('inspect', '--format', '{{json .NetworkSettings.Ports}}', name).stdout)
        base = 'http://127.0.0.1:' + ports['8787/tcp'][0]['HostPort']
        ready()
        docker('exec', name, '/ocd', '--no-update-check', 'instance', 'setup', '--yes', '--name', 'compat')
        token = docker('exec', name, 'cat', '/root/.open-compute/keys/admin.token').stdout.strip()
        accounts = request('/client/v4/accounts')
        assert len(accounts) == 1
        account = accounts[0]['id']
        prefix = f'/client/v4/accounts/{account}'
        namespace = request(prefix + '/storage/kv/namespaces', 'POST', {'title': 'centos-compat'})['id']
        value_path = prefix + f'/storage/kv/namespaces/{namespace}/values/state'
        request(value_path, 'PUT', b'ocd-centos82-worker-ok', 'text/plain')
        metadata = {'main_module': 'index.js', 'compatibility_date': '2026-09-08',
                    'bindings': [{'type': 'kv_namespace', 'name': 'DATA', 'namespace_id': namespace}]}
        source = 'export default {async fetch(request,env){return new Response(await env.DATA.get("state"));}}'
        boundary = 'centos82-' + uuid.uuid4().hex
        body = (f'--{boundary}\r\nContent-Disposition: form-data; name="metadata"\r\n'
                f'Content-Type: application/json\r\n\r\n{json.dumps(metadata)}\r\n'
                f'--{boundary}\r\nContent-Disposition: form-data; name="index.js"; filename="index.js"\r\n'
                f'Content-Type: application/javascript+module\r\n\r\n{source}\r\n--{boundary}--\r\n').encode()
        request(prefix + '/workers/scripts/centos82-worker', 'PUT', body, 'multipart/form-data; boundary=' + boundary)
        worker_host = f'centos82-worker.{account}.localhost'
        assert request('/', host=worker_host) == b'ocd-centos82-worker-ok'
        docker('restart', '--time', '20', name)
        ports = json.loads(docker('inspect', '--format', '{{json .NetworkSettings.Ports}}', name).stdout)
        base = 'http://127.0.0.1:' + ports['8787/tcp'][0]['HostPort']
        ready()
        assert request(value_path) == b'ocd-centos82-worker-ok'
        assert request('/', host=worker_host) == b'ocd-centos82-worker-ok'
        # This bare OS fixture mounts no Docker socket and installs no runtime libraries.
        docker('exec', name, 'sh', '-c', '! command -v docker; test ! -e /var/run/docker.sock')
        report = {'image': IMAGE, 'glibc': glibc, 'ocd': version,
                  'sha256': hashlib.sha256(BINARY.read_bytes()).hexdigest(),
                  'httpStatus': 200, 'response': 'ocd-centos82-worker-ok',
                  'kvSurvivedDaemonRestart': True, 'workerSurvivedDaemonRestart': True,
                  'dockerRequiredByDaemon': False, 'passed': True}
        (EVIDENCE / 'centos82-ocd-smoke.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report))
    finally:
        logs = docker('logs', name, check=False)
        # Tokens remain in the disposable container and never enter retained evidence.
        for suffix, content in [('stdout', logs.stdout), ('stderr', logs.stderr)]:
            (EVIDENCE / ('centos82-ocd.' + suffix)).write_text(content.replace(token, '[redacted]') if token else content)
        docker('rm', '--force', name, check=False)


if __name__ == '__main__':
    main()
