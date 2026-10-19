#!/usr/bin/env python3
"""Own one fresh CI Broker while an explicit Gate or coverage command runs."""
import argparse
import datetime
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
import tomllib

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('prepare_input', HERE / 'prepare-input.py')
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)
require = prepare.oci.require
CONFIG_FIELDS = {'socket', 'engine_socket', 'peer_uid', 'sidecar_image',
    'exec_helper_volume', 'exec_helper_sha256', 'qualification_image',
    'max_leases', 'max_instances', 'max_requests', 'relay_host', 'spool_dir',
    'state_dir', 'max_image_bytes', 'max_stream_bytes', 'resources'}
ASSET_ENV = ('OPEN_COMPUTE_TEST_WORKERD', 'OPEN_COMPUTE_BUILD_CADDY',
    'OPEN_COMPUTE_TEST_CADDY', 'OPEN_COMPUTE_TEST_BROWSER', 'OPEN_COMPUTE_BUILD_WORKERD_ARCHIVE',
    'OPEN_COMPUTE_TEST_OCD')
BUILD_ENV = ('RUSTUP_TOOLCHAIN', 'RUSTFLAGS', 'CARGO_TARGET_DIR', 'CARGO_BUILD_BUILD_DIR',
    'CARGO_BUILD_JOBS', 'CARGO_NET_OFFLINE', 'CARGO_INCREMENTAL', 'CARGO_PROFILE_DEV_DEBUG',
    'CARGO_PROFILE_TEST_DEBUG', 'CARGO_HOME', 'RUSTUP_HOME', 'MBX_CACHE_EXPORT_GROUP',
    'MBX_CACHE_LINKS', 'MALLOC_CONF', 'OPEN_COMPUTE_GATE_ROUNDS',
    'OPEN_COMPUTE_COVERAGE_HTML', 'GITHUB_ACTIONS', 'CI')


def command_environment(root, source):
    environment = {key: source[key] for key in ('PATH', 'HOME', 'USER', 'LOGNAME', 'LANG', 'TMPDIR',
        *ASSET_ENV, *BUILD_ENV) if key in source}
    environment.update(MBX_GC_AUTO='false', MBX_BUILD_SCRIPT_EXECUTION='0',
                       MBX_TARGET_VIEWS='0', MBX_RESTORE_HARDLINK='0',
                       DOCKER_CONFIG=str(root / 'docker-config'))
    return environment


def owned_config(path):
    require(os.getuid() != 0 and path.is_absolute() and path.resolve() == path,
            'ORDINARY_OWNER_AND_CANONICAL_CONFIG_REQUIRED')
    value = path.lstat()
    require(stat.S_ISREG(value.st_mode) and value.st_uid == os.getuid()
            and stat.S_IMODE(value.st_mode) == 0o600 and path.name == 'broker.toml',
            'PRIVATE_CONFIG_REQUIRED')
    parent = path.parent
    value = parent.lstat()
    require(stat.S_ISDIR(value.st_mode) and value.st_uid == os.getuid()
            and stat.S_IMODE(value.st_mode) == 0o700
            and {p.name for p in parent.iterdir()} == {'broker.toml'}, 'FRESH_PRIVATE_ROOT_REQUIRED')
    _, _, raw = prepare.oci.regular(path, 65536, True)
    config = tomllib.loads(raw.decode())
    require(set(config) == CONFIG_FIELDS and type(config['peer_uid']) is int
            and config['peer_uid'] == os.getuid(),
            'CLOSED_CONFIG_AND_PEER_UID_REQUIRED')
    for key, name in [('socket', 'broker.sock'), ('state_dir', 'state'), ('spool_dir', 'spool')]:
        require(config[key] == str(parent / name), 'OWNED_PATH_REQUIRED')
    require(len(os.fsencode(config['socket'])) <= 103, 'BROKER_SOCKET_PATH_TOO_LONG')
    engine = Path(config['engine_socket'])
    require(engine.is_absolute() and engine.resolve() == engine
            and len(os.fsencode(engine)) <= 103, 'ENGINE_SOCKET_PATH')
    value = engine.lstat()
    require(stat.S_ISSOCK(value.st_mode) and value.st_uid in (0, os.getuid())
            and value.st_mode & 0o002 == 0, 'ENGINE_SOCKET_AUTHORITY')
    return config, raw


def clone_binary(source, expected, destination):
    require(re.fullmatch('[0-9a-f]{64}', expected) is not None, 'BINARY_DIGEST_REQUIRED')
    require(source.is_absolute() and source.resolve() == source, 'CANONICAL_BINARY_PATH')
    source_fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(source_fd, 'rb') as input_file:
        before = os.fstat(input_file.fileno())
        require(stat.S_ISREG(before.st_mode) and before.st_mode & 0o111
                and 0 < before.st_size <= 1 << 30, 'BINARY_DIGEST_OR_MODE')
        output_fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        digest = hashlib.sha256()
        copied = 0
        with os.fdopen(output_fd, 'wb') as output_file:
            while chunk := input_file.read(1 << 20):
                copied += len(chunk)
                require(copied <= before.st_size, 'BINARY_CHANGED')
                digest.update(chunk)
                output_file.write(chunk)
            output_file.flush()
            os.fsync(output_file.fileno())
        require(digest.hexdigest() == expected and copied == before.st_size
                and prepare.oci.identity(before) == prepare.oci.identity(os.fstat(input_file.fileno()))
                and prepare.oci.identity(before) == prepare.oci.identity(source.lstat()), 'BINARY_CHANGED')
    destination.chmod(0o555)
    actual, _, _ = prepare.oci.regular(destination, 1 << 30)
    require(actual == expected and prepare.oci.identity(before) == prepare.oci.identity(source.lstat())
            and destination.stat().st_nlink == 1
            and (destination.stat().st_dev, destination.stat().st_ino)
                != (source.stat().st_dev, source.stat().st_ino), 'BINARY_CLONE_CHANGED')


def stop_child(process):
    record = {'pid': process.pid, 'terminateSent': False, 'killSent': False}
    if process.poll() is None:
        process.terminate()
        record['terminateSent'] = True
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            record['killSent'] = True
    record['exitCode'] = process.wait(timeout=5)
    record['reaped'] = True
    return record


def inventory(config, environment):
    result = subprocess.run(['docker', '--host', 'unix://' + config['engine_socket'],
        'container', 'ls', '--all', '--quiet', '--no-trunc'], env=environment,
        capture_output=True, check=True)
    rows = result.stdout.decode().splitlines()
    require(all(re.fullmatch('[0-9a-f]{64}', row) for row in rows), 'ENGINE_INVENTORY_FORMAT')
    return sorted(rows)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--binary-sha256', required=True)
    parser.add_argument('--config', type=Path, required=True)
    parser.add_argument('--policy', type=Path, required=True)
    parser.add_argument('--images', type=Path, nargs=2, required=True)
    parser.add_argument('--command', nargs=argparse.REMAINDER, required=True)
    args = parser.parse_args()
    require(bool(args.command), 'EXPLICIT_COMMAND_REQUIRED')
    require(not (Path.cwd() / '.env').exists(), 'CLEAN_CI_CHECKOUT_REQUIRED')
    config, config_raw = owned_config(args.config)
    root = args.config.parent
    evidence = root / 'evidence'
    evidence.mkdir(mode=0o700)
    environment = command_environment(root, os.environ)
    Path(environment['DOCKER_CONFIG']).mkdir(mode=0o700)
    result = {'success': False, 'startedAtUtc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
        'mainExecuted': False, 'providerStarted': False, 'cleanupDeletesEngineResources': False,
        'proofScope': 'Owned Broker lifecycle and wrapped command exit',
        'fullWorkspaceOrCompatibilityQualifiedByWrapper': False}
    process = None
    stdout = stderr = None
    before = None
    try:
        clone = root / 'ocd'
        clone_binary(args.binary, args.binary_sha256, clone)
        before = inventory(config, environment)
        require(not before, 'EMPTY_DEDICATED_CI_ENGINE_REQUIRED')
        result['inventoryBeforeSha256'] = hashlib.sha256(json.dumps(before).encode()).hexdigest()
        (root / 'state').mkdir(mode=0o700)
        (root / 'spool').mkdir(mode=0o700)
        stdout = (evidence / 'broker.stdout').open('xb')
        stderr = (evidence / 'broker.stderr').open('xb')
        process = subprocess.Popen([str(clone), 'container-broker', '--broker-config', str(args.config)],
            env=environment, stdout=stdout, stderr=stderr, start_new_session=True)
        result['providerStarted'] = True
        result['ownedProviderPid'] = process.pid
        deadline = time.monotonic() + 240
        while True:
            require(process.poll() is None, 'BROKER_EXITED_BEFORE_READY')
            try:
                body, authority = prepare.read_contract(Path(config['socket']))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                require(time.monotonic() < deadline, 'BROKER_READINESS_DEADLINE')
                time.sleep(0.1)
        require(inventory(config, environment) == before, 'QUALIFICATION_CONTAINER_LEAK')
        prepared = subprocess.run([sys.executable, '-I', '-B', str(HERE / 'prepare-input.py'),
            '--broker-socket', config['socket'], '--policy', str(args.policy),
            *map(str, args.images)], env=environment, capture_output=True)
        require(prepared.returncode == 0, 'INPUT_PREPARATION_FAILED')
        input_path = root / 'input.json'
        fd = os.open(input_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, 'wb') as stream:
            stream.write(prepared.stdout)
            stream.flush()
            os.fsync(stream.fileno())
        environment['OPEN_COMPUTE_TEST_CONTAINERS_INPUT'] = str(input_path)
        result['inputSha256'] = hashlib.sha256(prepared.stdout).hexdigest()
        result['mainExecuted'] = True
        with (evidence / 'command.log').open('xb') as log:
            done = subprocess.run(args.command, env=environment, stdout=log, stderr=subprocess.STDOUT)
        result['mainExitCode'] = done.returncode
        require(prepare.oci.regular(clone, 1 << 30)[0] == args.binary_sha256, 'BINARY_CHANGED')
        require(process.poll() is None and prepare.read_contract(Path(config['socket'])) == (body, authority),
                'PROVIDER_CHANGED_DURING_COMMAND')
        after = inventory(config, environment)
        result['inventoryAfterSha256'] = hashlib.sha256(json.dumps(after).encode()).hexdigest()
        require(before == after, 'COMMAND_CONTAINER_LEAK')
        require(args.config.read_bytes() == config_raw, 'CONFIG_CHANGED')
        result['success'] = done.returncode == 0
    except Exception as error:
        result['failureClass'] = type(error).__name__
        code = str(error) if isinstance(error, ValueError) else ''
        result['failureCode'] = code if re.fullmatch('[A-Z_]+', code) else None
    finally:
        if process is not None:
            try:
                result['ownedProviderCleanup'] = stop_child(process)
                cleanup = result['ownedProviderCleanup']
                result['success'] = (result['success'] and cleanup['terminateSent']
                                     and not cleanup['killSent'] and cleanup['exitCode'] == 0)
            except Exception as error:
                result['cleanupFailureClass'] = type(error).__name__
                result['success'] = False
        result['brokerSocketAbsentAfterCleanup'] = not os.path.lexists(config['socket'])
        result['success'] = result['success'] and result['brokerSocketAbsentAfterCleanup']
        if before is not None:
            try:
                result['inventoryAfterCleanupMatches'] = inventory(config, environment) == before
                result['success'] = result['success'] and result['inventoryAfterCleanupMatches']
            except Exception as error:
                result['inventoryCleanupFailureClass'] = type(error).__name__
                result['success'] = False
        if stdout is not None:
            stdout.close()
        if stderr is not None:
            stderr.close()
        result['finishedAtUtc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
        (evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))
    return 0 if result['success'] else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        code = str(error) if isinstance(error, ValueError) else ''
        print(json.dumps({'success': False, 'failureBeforeCompleteResultClass': type(error).__name__,
                          'failureCode': code if re.fullmatch('[A-Z_]+', code) else None}))
        raise SystemExit(1)
