#!/usr/bin/env python3
"""Copy a verified sidecar helper into a new, labelled local Engine volume."""
import argparse
import hashlib
import http.client
import io
import json
import os
from pathlib import Path
import re
import socket
import stat
import tarfile
import uuid

DIRECTORY = '/.open-compute-exec'
LIMIT = 16 << 20


def require(condition, code):
    if not condition:
        raise ValueError(code)


class Engine:
    def __init__(self, path):
        require(path.is_absolute() and path.resolve() == path
                and len(os.fsencode(path)) <= 103, 'ENGINE_SOCKET_PATH')
        self.path = path
        self.identity = path.lstat()
        require(stat.S_ISSOCK(self.identity.st_mode)
                and self.identity.st_uid in (0, os.getuid())
                and self.identity.st_mode & 0o002 == 0, 'ENGINE_SOCKET_AUTHORITY')

    def unchanged(self):
        current = self.path.lstat()
        keys = ('st_dev', 'st_ino', 'st_uid', 'st_gid', 'st_mode', 'st_nlink',
                'st_size', 'st_mtime_ns', 'st_ctime_ns')
        require(all(getattr(current, key) == getattr(self.identity, key) for key in keys),
                'ENGINE_SOCKET_CHANGED')

    def request(self, method, path, body=None, archive=False):
        self.unchanged()
        connection = http.client.HTTPConnection('localhost', timeout=30)
        channel = socket.socket(socket.AF_UNIX)
        channel.settimeout(30)
        connection.sock = channel
        try:
            channel.connect(str(self.path))
            data = body if archive else json.dumps(body).encode() if body is not None else None
            connection.request(method, path, data,
                {'Content-Type': 'application/x-tar' if archive else 'application/json'})
            response = connection.getresponse()
            data = response.read(LIMIT + 65537)
            require(len(data) <= LIMIT + 65536, 'ENGINE_RESPONSE_LIMIT')
            self.unchanged()
            return response.status, data if archive else json.loads(data) if data else None
        finally:
            connection.close()

    def inventory(self):
        result = {}
        for name, path, key in [('containers', '/containers/json?all=true', 'Id'),
                ('images', '/images/json?all=true', 'Id'),
                ('networks', '/networks', 'Id'), ('volumes', '/volumes', 'Name')]:
            status, rows = self.request('GET', path)
            require(status == 200, 'ENGINE_INVENTORY_STATUS')
            if name == 'volumes':
                rows = rows['Volumes'] or []
            result[name] = sorted({row[key] for row in rows})
        return result


def verify_archive(data, expected, name='helper'):
    require(len(data) <= LIMIT + 65536, 'HELPER_ARCHIVE_LIMIT')
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:') as archive:
        rows = archive.getmembers()
        require(len(rows) == 1, 'HELPER_ARCHIVE_ENTRIES')
        row = rows[0]
        require(row.name == name and row.isreg() and 0 < row.size <= LIMIT,
                'HELPER_ARCHIVE_TYPE')
        require(row.mode & 0o555 == 0o555 and row.mode & 0o7000 == 0,
                'HELPER_ARCHIVE_MODE')
        with archive.extractfile(row) as source:
            require(hashlib.file_digest(source, 'sha256').hexdigest() == expected,
                    'HELPER_ARCHIVE_DIGEST')
        return row.size


def materialize(engine, image, helper_sha, volume, architecture):
    before = engine.inventory()
    require(not before['containers'] and volume not in before['volumes'],
            'EMPTY_ENGINE_AND_NEW_VOLUME_REQUIRED')
    status, description = engine.request('GET', f'/images/{image}/json')
    require(status == 200 and description['Id'] == image
            and description['Os'] == 'linux' and description['Architecture'] == architecture,
            'SIDECAR_IMAGE_IDENTITY')
    require(not description['Config'].get('Volumes'), 'ANONYMOUS_VOLUME_FORBIDDEN')
    labels = {'dev.open-compute.exec-helper.image': image,
              'dev.open-compute.exec-helper.sha256': helper_sha}
    owned = []
    created_volume = False
    result = {'success': False, 'image': image, 'helperSha256': helper_sha,
              'volume': volume, 'providerQualified': False, 'containersStarted': False}
    try:
        def create(mounts):
            status, value = engine.request('POST', '/containers/create?name=oc-helper-materialize-' + uuid.uuid4().hex,
                {'Image': image, 'HostConfig': {'NetworkMode': 'none', 'Mounts': mounts,
                 'RestartPolicy': {'Name': 'no'}, 'LogConfig': {'Type': 'none'}}})
            require(status == 201 and re.fullmatch('[0-9a-f]{64}', value['Id']), 'HELPER_CONTAINER_CREATE')
            owned.append(value['Id'])
            return value['Id']

        source = create([])
        status, data = engine.request('GET', f'/containers/{source}/archive?path={DIRECTORY}/helper', archive=True)
        require(status == 200, 'HELPER_SOURCE_ARCHIVE_STATUS')
        size = verify_archive(data, helper_sha)
        status, proxy = engine.request('GET', f'/containers/{source}/archive?path=/proxy-everything', archive=True)
        require(status == 200 and verify_archive(proxy, helper_sha, 'proxy-everything') == size,
                'SIDECAR_AND_HELPER_DIFFER')
        status, value = engine.request('POST', '/volumes/create',
            {'Name': volume, 'Driver': 'local', 'Labels': labels})
        require(status == 201 and value['Name'] == volume and value['Labels'] == labels
                and value['Driver'] == 'local' and value['Scope'] == 'local'
                and not value.get('Options'), 'HELPER_VOLUME_CREATE')
        created_volume = True
        writer = create([{'Type': 'volume', 'Source': volume, 'Target': DIRECTORY,
                          'ReadOnly': False, 'VolumeOptions': {'NoCopy': True}}])
        status, _ = engine.request('PUT', f'/containers/{writer}/archive?path={DIRECTORY}', data, archive=True)
        require(status == 200, 'HELPER_VOLUME_WRITE')
        status, proof = engine.request('GET', f'/containers/{writer}/archive?path={DIRECTORY}/helper', archive=True)
        require(status == 200 and verify_archive(proof, helper_sha) == size, 'HELPER_VOLUME_READBACK')
        result.update(success=True, helperBytes=size)
    except Exception as error:
        result['failureClass'] = type(error).__name__
        result['failureCode'] = str(error) if isinstance(error, ValueError) else None
    finally:
        try:
            for identity in reversed(owned):
                status, _ = engine.request('DELETE', f'/containers/{identity}')
                require(status in (204, 404), 'HELPER_CONTAINER_DELETE')
                require(engine.request('GET', f'/containers/{identity}/json')[0] == 404,
                        'HELPER_CONTAINER_REMAINS')
            result['allOwnedContainersAbsent'] = True
            if created_volume:
                status, value = engine.request('GET', f'/volumes/{volume}')
                require(status == 200 and value['Name'] == volume and value['Labels'] == labels
                        and value['Driver'] == 'local' and value['Scope'] == 'local'
                        and not value.get('Options'), 'HELPER_VOLUME_CHANGED')
                if not result['success']:
                    require(engine.request('DELETE', f'/volumes/{volume}')[0] == 204,
                            'FAILED_HELPER_VOLUME_DELETE')
                    require(engine.request('GET', f'/volumes/{volume}')[0] == 404,
                            'FAILED_HELPER_VOLUME_REMAINS')
            after = engine.inventory()
            wanted = dict(before)
            if result['success']:
                wanted['volumes'] = sorted(before['volumes'] + [volume])
            require(after == wanted, 'HELPER_MATERIALIZATION_INVENTORY_CHANGED')
            engine.unchanged()
            result.update(inventoryBefore=before, inventoryAfter=after,
                          volumeRetained=result['success'], engineSocketUnchanged=True)
        except Exception as error:
            result.update(success=False, cleanupFailureClass=type(error).__name__,
                cleanupFailureCode=str(error) if isinstance(error, ValueError) else None)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine-socket', required=True, type=Path)
    parser.add_argument('--image', required=True)
    parser.add_argument('--helper-sha256', required=True)
    parser.add_argument('--volume', required=True)
    parser.add_argument('--architecture', required=True, choices=('arm64', 'amd64'))
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    require(os.getuid() == os.geteuid() != 0, 'ORDINARY_OPERATOR_REQUIRED')
    require(re.fullmatch('sha256:[0-9a-f]{64}', args.image)
            and re.fullmatch('[0-9a-f]{64}', args.helper_sha256)
            and re.fullmatch('[a-zA-Z0-9][a-zA-Z0-9_.-]{0,127}', args.volume), 'HELPER_INPUT_IDENTITY')
    require(args.output.is_absolute() and args.output.parent.resolve() == args.output.parent
            and not os.path.lexists(args.output), 'FRESH_HELPER_PROOF_REQUIRED')
    result = materialize(Engine(args.engine_socket), args.image, args.helper_sha256, args.volume, args.architecture)
    fd = os.open(args.output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'w') as output:
        json.dump(result, output, indent=2)
        output.write('\n')
        output.flush()
        os.fsync(output.fileno())
    print(json.dumps({'success': result['success'], 'volume': args.volume,
                      'providerQualified': False}))
    return 0 if result['success'] else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except Exception as error:
        print(json.dumps({'success': False, 'failureClass': type(error).__name__,
              'failureCode': str(error) if isinstance(error, ValueError) else None}))
        raise SystemExit(1)
