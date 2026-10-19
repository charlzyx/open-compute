"""Run the rebuilt runtime and a real Worker in original CentOS 8.2 userspace."""
import argparse
import json
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request
import uuid

IMAGE = 'centos@sha256:fd84102fc72960dd1b8da0ee3b4c13e3b0c1d2a085de118bc4c97821cd986e02'
ROOT = Path(__file__).resolve().parents[2]
arguments = argparse.ArgumentParser()
arguments.add_argument('--allocator', choices=['tcmalloc', 'system'], default='tcmalloc')
allocator = arguments.parse_args().allocator
EVIDENCE = ROOT / ('.temp/glibc-compat-system-malloc/evidence' if allocator == 'system' else '.temp/glibc-compat/evidence')


def docker(*args, check=True):
    return subprocess.run(['docker', *args], capture_output=True, text=True,
                          timeout=60, check=check)


def main():
    docker('pull', IMAGE)
    environment = docker('run', '--rm', '--network', 'none', IMAGE,
                         'getconf', 'GNU_LIBC_VERSION').stdout.strip()
    if environment != 'glibc 2.28':
        raise AssertionError('CentOS smoke requires the exact glibc 2.28 baseline')
    kernel = docker('run', '--rm', '--network', 'none', IMAGE, 'uname', '-sr').stdout.strip()
    binary = EVIDENCE / 'workerd'
    if binary.stat().st_mode & 0o111 == 0:
        raise AssertionError('The rebuilt workerd must be executable')
    mounts = ['--mount', f'type=bind,source={binary},target=/workerd,readonly',
              '--mount', f'type=bind,source={ROOT / "test/glibc-compat/worker.capnp"},target=/worker.capnp,readonly']
    version = docker('run', '--rm', '--network', 'none', *mounts, IMAGE,
                     '/workerd', '--version').stdout.strip()
    if version != 'workerd 2026-09-30':
        raise AssertionError('Unexpected workerd version in CentOS 8.2')
    name = 'oc-glibc228-' + uuid.uuid4().hex
    try:
        docker('run', '--detach', '--name', name, '--read-only',
               '--cap-drop=ALL', '--security-opt=no-new-privileges',
               '--user', '65534:65534', '--publish', '127.0.0.1::8080',
               *mounts, IMAGE, '/workerd', 'serve', '/worker.capnp',
               '--experimental', '--socket-addr=http=0.0.0.0:8080')
        ports = json.loads(docker('inspect', '--format',
                                  '{{json .NetworkSettings.Ports}}', name).stdout)
        port = int(ports['8080/tcp'][0]['HostPort'])
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:{port}', timeout=1) as response:
                    body = response.read()
                    if response.status != 200 or body != b'glibc228-worker-ok':
                        raise AssertionError('CentOS Worker response did not match')
                    report = {'image': IMAGE, 'glibc': environment, 'workerd': version,
                              'allocator': allocator, 'hostSharedKernel': kernel,
                              'httpStatus': response.status, 'response': body.decode(),
                              'passed': True}
                    (EVIDENCE / 'centos82-smoke.json').write_text(json.dumps(report, indent=2) + '\n')
                    print(json.dumps(report))
                    return
            except (OSError, urllib.error.URLError):
                if docker('inspect', '--format', '{{.State.Running}}', name).stdout.strip() != 'true':
                    raise AssertionError('workerd exited before the CentOS HTTP probe') from None
                time.sleep(0.1)
        raise AssertionError('CentOS Worker readiness deadline expired')
    finally:
        logs = docker('logs', name, check=False)
        (EVIDENCE / 'centos82-worker.stdout').write_text(logs.stdout)
        (EVIDENCE / 'centos82-worker.stderr').write_text(logs.stderr)
        docker('rm', '--force', name, check=False)


if __name__ == '__main__':
    main()
