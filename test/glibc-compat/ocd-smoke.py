"""Exercise packaged Workers, static assets and persistence in CentOS 8.2."""
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


def exercise(image, volume):
    identity = docker('run', '--rm', '--network', 'none', image, 'sh', '-c',
                      'printf "%s\\n" "$(id -u)" "$(id -g)" "$HOME" "$(getent passwd "$(id -u)" | cut -d: -f6)" "$(stat -c %u "$HOME")" "$(stat -c %a "$HOME")"').stdout.splitlines()
    assert identity == ['10001', '10001', '/home/occompat', '/home/occompat', '10001', '700'], 'UID, passwd home and ownership must agree'
    mounts = ['--mount', f'type=bind,source={BINARY},target=/ocd,readonly',
              '--mount', f'type=volume,source={volume},target=/home/occompat/.open-compute']
    glibc = docker('run', '--rm', '--network', 'none', image, 'getconf', 'GNU_LIBC_VERSION').stdout.strip()
    assert glibc == 'glibc 2.28'
    version = docker('run', '--rm', '--network', 'none', *mounts, image, '/ocd', '--no-update-check', '--version').stdout.strip()
    name = 'ocd-centos82-' + uuid.uuid4().hex
    token = ''
    base = ''
    passed = False

    def state():
        raw = json.loads(docker('inspect', name).stdout)[0]
        return {'state': {key: raw['State'][key] for key in
                          ('Status', 'Running', 'ExitCode', 'OOMKilled', 'Error')},
                'ports': raw['NetworkSettings']['Ports'],
                'configuredPorts': raw['HostConfig']['PortBindings'],
                'configuredUser': raw['Config']['User'],
                'tmpfs': raw['HostConfig'].get('Tmpfs'),
                'mounts': [{'type': item['Type'], 'destination': item['Destination'],
                            'name': item.get('Name'), 'rw': item['RW']} for item in raw['Mounts']]}

    def endpoint():
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            current = state()
            if not current['state']['Running']:
                raise AssertionError('CentOS daemon exited before port publication: ' + json.dumps(current['state']))
            ports = current['ports']
            if '8787/tcp' in ports and ports['8787/tcp']:
                binding = ports['8787/tcp'][0]
                assert binding['HostIp'] == '127.0.0.1', 'Smoke port must remain loopback-only'
                port = int(binding['HostPort'])
                assert 1 <= port <= 65535
                return 'http://127.0.0.1:' + str(port)
            time.sleep(0.1)
        raise AssertionError('Docker did not publish the configured smoke port')

    def request(path, method='GET', body=None, content_type='application/json', host=None,
                credential=None, status=200):
        # The TCP destination is a Docker-published port; authority must still
        # identify the actual 8787 listener checked by SharedRoutes.
        headers = {'Host': host or 'localhost:8787', 'Content-Type': content_type}
        if host is None:
            headers['Authorization'] = 'Bearer ' + (credential or token)
        data = json.dumps(body).encode() if isinstance(body, (dict, list)) else body
        req = urllib.request.Request(base + path, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=15) as response:
                payload = response.read()
                assert response.status == status
        except urllib.error.HTTPError as error:
            detail = error.read(4096).decode(errors='replace')
            for secret in (token, credential):
                if secret:
                    detail = detail.replace(secret, '[redacted]')
            raise AssertionError(f'{method} {path} Host={headers["Host"]} returned {error.code}: {detail}') from None
        if host or '/values/' in path:
            return payload
        value = json.loads(payload)
        assert value['success']
        return value['result']

    def ready(expected_instance=None):
        deadline = time.monotonic() + 90
        last_instances = None
        while time.monotonic() < deadline:
            try:
                with urllib.request.urlopen(base + '/health/ready', timeout=1) as response:
                    healthy = response.status == 200
                if healthy and expected_instance is None:
                    return
                if healthy:
                    req = urllib.request.Request(base + '/operator/api/instances', headers={
                        'Host': 'localhost:8787', 'Authorization': 'Bearer ' + token})
                    with urllib.request.urlopen(req, timeout=2) as response:
                        last_instances = json.loads(response.read())['instances']
                    matching = [entry for entry in last_instances if entry['instance_id'] == expected_instance]
                    if len(matching) == 1:
                        if matching[0]['state'] == 'failed':
                            raise AssertionError('Restored instance failed admission: ' + json.dumps(matching[0]))
                        if matching[0]['state'] == 'running':
                            return
            except (OSError, urllib.error.URLError):
                pass
            if not state()['state']['Running']:
                raise AssertionError('CentOS daemon exited before instance readiness')
            time.sleep(0.2)
        raise AssertionError('CentOS instance readiness deadline expired: ' + json.dumps(last_instances))

    def persisted_identity():
        return docker('exec', name, 'sh', '-c',
                      'sha256sum "$HOME/.open-compute/ocd.toml" "$HOME/.open-compute/keys/admin.token" "$HOME/.open-compute/instances/compat/compute.toml"').stdout

    bootstrap = '''set -eu
umask 077
test "$(id -u)" = 10001
test "$HOME" = /home/occompat
if [ ! -e "$HOME/.open-compute/ocd.toml" ]; then
  mkdir -p "$HOME/.open-compute/keys"
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \\n' > "$HOME/.open-compute/keys/admin.token"
  printf '[server]\\npublic_bind = "0.0.0.0:8787"\\nadmin_auth = { file = "./keys/admin.token" }\\n' > "$HOME/.open-compute/ocd.toml"
fi
test "$(stat -c %u "$HOME/.open-compute")" = "$(id -u)"
test "$(stat -c %a "$HOME/.open-compute")" = 700
exec /ocd --no-update-check run
'''
    try:
        docker('run', '--detach', '--name', name, '--cap-drop=ALL',
               '--security-opt=no-new-privileges', '--publish', '127.0.0.1::8787',
               *mounts, image, 'sh', '-c', bootstrap)
        base = endpoint()
        mounted = state()
        assert any(item['type'] == 'volume' and item['name'] == volume and item['destination'] == '/home/occompat/.open-compute' and item['rw'] for item in mounted['mounts'])
        assert not mounted['tmpfs'], 'Persistent smoke authority must not use tmpfs'
        ready()
        docker('exec', name, '/ocd', '--no-update-check', 'instance', 'setup', '--yes', '--name', 'compat')
        token = docker('exec', name, 'cat', '/home/occompat/.open-compute/keys/admin.token').stdout.strip()
        accounts = request('/client/v4/accounts')
        assert len(accounts) == 1
        account = accounts[0]['id']
        instances = json.loads(docker('exec', name, '/ocd', '--no-update-check', 'instances', '--json').stdout)['instances']
        assert len(instances) == 1 and instances[0]['state'] == 'running'
        assert account == instances[0]['instance_id'], 'Discovery must identify the initialized instance'
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
        # Local-origin Host follows the installed-product API; omit the port
        # while the TCP connection uses Docker's explicit published endpoint.
        worker_host = f'centos82-worker.{account}.localhost'
        assert request('/', host=worker_host) == b'ocd-centos82-worker-ok'
        def upload_static(script):
            # The fixed /index.html fixture uses Wrangler's BLAKE3 asset identity.
            asset_hash = '4c73266e449fea54bba5a6dea074dbbd'
            session = request(prefix + f'/workers/scripts/{script}/assets-upload-session',
                              'POST', {'manifest': {'/index.html': {'hash': asset_hash, 'size': 4}}})
            completion = session['jwt']
            if session['buckets']:
                completion = request(prefix + '/workers/assets/upload/' + asset_hash,
                                     'POST', b'good', 'text/html', credential=completion,
                                     status=201)['jwt']
            static_metadata = {'compatibility_date': '2026-09-08',
                               'assets': {'jwt': completion, 'config': {}}}
            static_body = (f'--{boundary}\r\nContent-Disposition: form-data; name="metadata"\r\n'
                           f'Content-Type: application/json\r\n\r\n{json.dumps(static_metadata)}\r\n'
                           f'--{boundary}--\r\n').encode()
            request(prefix + f'/workers/scripts/{script}', 'PUT', static_body,
                    'multipart/form-data; boundary=' + boundary)
            assert request('/', host=f'{script}.{account}.localhost') == b'good'
            assert request(prefix + f'/workers/scripts/{script}/schedules') == {'schedules': []}

        upload_static('centos82-static')
        # Metadata cloning must not invent a Cron configuration for assets-only content.
        request(prefix + '/workers/scripts/centos82-static/schedules', 'PUT', [])
        upload_static('centos82-static')
        scheduled_source = 'export default {fetch(){return new Response("scheduled")},scheduled(){}}'
        scheduled_body = body.replace(source.encode(), scheduled_source.encode())
        request(prefix + '/workers/scripts/centos82-cron-static', 'PUT', scheduled_body,
                'multipart/form-data; boundary=' + boundary)
        request(prefix + '/workers/scripts/centos82-cron-static/schedules', 'PUT', [{'cron': '0 0 * * *'}])
        assert len(request(prefix + '/workers/scripts/centos82-cron-static/schedules')['schedules']) == 1
        upload_static('centos82-cron-static')
        print('Pure assets deploy, redeploy, metadata clone and Cron retirement verified', flush=True)
        before_restart = persisted_identity()
        print('Real Worker KV response verified before daemon restart', flush=True)
        docker('restart', '--time', '20', name)
        base = endpoint()
        assert persisted_identity() == before_restart, 'Persistent configuration or credentials changed across restart'
        ready(account)
        assert request(value_path) == b'ocd-centos82-worker-ok'
        assert request('/', host=worker_host) == b'ocd-centos82-worker-ok'
        for script in ('centos82-static', 'centos82-cron-static'):
            assert request('/', host=f'{script}.{account}.localhost') == b'good'
            assert request(prefix + f'/workers/scripts/{script}/schedules') == {'schedules': []}
        # This bare OS fixture mounts no Docker socket and installs no runtime libraries.
        docker('exec', name, 'sh', '-c', '! command -v docker; test ! -e /var/run/docker.sock')
        report = {'baseImage': IMAGE, 'image': docker('image', 'inspect', '--format', '{{.Id}}', image).stdout.strip(),
                  'uid': int(identity[0]), 'home': identity[2], 'ocdDir': identity[2] + '/.open-compute',
                  'glibc': glibc, 'ocd': version,
                  'sha256': hashlib.sha256(BINARY.read_bytes()).hexdigest(),
                  'controlHost': 'localhost:8787', 'workerHost': worker_host,
                  'httpStatus': 200, 'response': 'ocd-centos82-worker-ok',
                  'storageMountType': 'volume', 'persistentIdentityUnchanged': True,
                  'restoredInstanceRunning': True,
                  'kvSurvivedDaemonRestart': True, 'workerSurvivedDaemonRestart': True,
                  'pureAssetsSurvivedDaemonRestart': True, 'workerCronToAssetsOnly': True,
                  'dockerRequiredByDaemon': False, 'passed': True}
        (EVIDENCE / 'centos82-ocd-smoke.json').write_text(json.dumps(report, indent=2) + '\n')
        print(json.dumps(report))
        passed = True
    finally:
        current = state()
        (EVIDENCE / 'centos82-ocd-state.json').write_text(json.dumps(current, indent=2) + '\n')
        if not passed:
            print('CentOS container state: ' + json.dumps(current), flush=True)
        logs = docker('logs', name, check=False)
        # Tokens remain in the disposable container and never enter retained evidence.
        for suffix, content in [('stdout', logs.stdout), ('stderr', logs.stderr)]:
            sanitized = content.replace(token, '[redacted]') if token else content
            (EVIDENCE / ('centos82-ocd.' + suffix)).write_text(sanitized)
            if not passed and sanitized:
                print('CentOS daemon ' + suffix + ':\n' + sanitized[-8000:], flush=True)
        docker('rm', '--force', name, check=False)


def main():
    image = 'ocd-centos82-smoke:' + uuid.uuid4().hex
    docker('build', '--tag', image, '--file', str(ROOT / 'test/glibc-compat/Dockerfile.smoke'), str(ROOT / 'test/glibc-compat'))
    volume = 'ocd-centos82-data-' + uuid.uuid4().hex
    docker('volume', 'create', volume)
    try:
        exercise(image, volume)
    finally:
        docker('volume', 'rm', volume, check=False)
        docker('image', 'rm', image, check=False)


if __name__ == '__main__':
    main()
