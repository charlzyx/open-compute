#!/usr/bin/env python3
"""Read an active Broker contract and emit bounded P23 test inputs."""
import argparse
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import re
import socket
import stat
import sys

spec = importlib.util.spec_from_file_location('oci_images', Path(__file__).with_name('oci-images.py'))
oci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oci)
FIELDS = {'version', 'runtime', 'engineVersion', 'architecture', 'isolation',
          'qualified', 'snapshots', 'sidecarImage', 'execHelperSha256', 'resources',
          'maxInstances', 'maxImageBytes', 'maxStreamBytes'}
RESOURCE_KEYS = {'cpuNanos', 'memoryBytes', 'diskBytes', 'pids', 'openFiles'}


def integer(value, low, high):
    return type(value) is int and low <= value <= high


def policy(body, broker_socket, requested):
    oci.require(isinstance(requested, dict) and set(requested) == {
        'max_instances', 'max_snapshots', 'max_snapshot_bytes', 'resources'}, 'INPUT_POLICY_FIELDS')
    instances, snapshots, snapshot_bytes = (requested[name] for name in (
        'max_instances', 'max_snapshots', 'max_snapshot_bytes'))
    contract = json.loads(body, object_pairs_hook=oci.pairs)
    oci.require(isinstance(contract, dict) and set(contract) == FIELDS, 'CONTRACT_FIELDS')
    oci.require(type(contract['version']) is int and contract['version'] == 4
                and contract['runtime'] == 'docker' and contract['qualified'] is True
                and contract['snapshots'] is True, 'CONTRACT_NOT_QUALIFIED')
    oci.require(all(isinstance(contract[name], str) and contract[name]
                    for name in ('engineVersion', 'isolation'))
                and all(integer(contract[name], 1, 1 << 40)
                        for name in ('maxImageBytes', 'maxStreamBytes')), 'CONTRACT_POLICY')
    oci.require(contract['architecture'] in ('arm64', 'amd64')
                and isinstance(contract['sidecarImage'], str)
                and re.fullmatch(r'sha256:[0-9a-f]{64}', contract['sidecarImage'])
                and isinstance(contract['execHelperSha256'], str)
                and re.fullmatch(r'[0-9a-f]{64}', contract['execHelperSha256']), 'CONTRACT_PLATFORM_OR_HELPER')
    resources = contract['resources']
    oci.require(isinstance(resources, dict) and set(resources) == RESOURCE_KEYS
                and all(integer(resources[name], 1, (1 << 63) - 1)
                        for name in ('cpuNanos', 'memoryBytes', 'diskBytes'))
                and all(integer(resources[name], 1, (1 << 32) - 1)
                        for name in ('pids', 'openFiles')), 'CONTRACT_RESOURCES')
    oci.require(integer(contract['maxInstances'], 2, 1024)
                and integer(instances, 2, contract['maxInstances'])
                and integer(snapshots, 1, 100000)
                and integer(snapshot_bytes, 1, (1 << 63) - 1), 'INPUT_LIMITS')
    selected = requested['resources']
    oci.require(isinstance(selected, dict) and set(selected) == RESOURCE_KEYS
                and all(integer(selected[name], 1, resources[name]) for name in RESOURCE_KEYS),
                'INPUT_RESOURCE_LIMITS')
    return {'broker_socket': str(broker_socket),
            'provider_contract_sha256': hashlib.sha256(body).hexdigest(),
            'max_instances': instances, 'max_snapshots': snapshots,
            'max_snapshot_bytes': snapshot_bytes, 'resources': selected}, contract['architecture']


def read_contract(path):
    oci.require(path.is_absolute() and path.resolve() == path
                and len(os.fsencode(path)) <= 103, 'BROKER_SOCKET_PATH')
    before = path.lstat()
    oci.require(stat.S_ISSOCK(before.st_mode) and before.st_uid == os.getuid()
                and stat.S_IMODE(before.st_mode) == 0o600, 'BROKER_SOCKET_AUTHORITY')
    connection = http.client.HTTPConnection('localhost', timeout=240)
    channel = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    channel.settimeout(240)
    connection.sock = channel
    try:
        channel.connect(str(path))
        connection.request('GET', '/open-compute/v1/contract')
        response = connection.getresponse()
        body = response.read(65537)
        oci.require(response.status == 200 and 0 < len(body) <= 65536,
                    'BROKER_CONTRACT_RESPONSE')
        oci.require(oci.identity(before) == oci.identity(path.lstat()), 'BROKER_SOCKET_CHANGED')
        return body, oci.identity(before)
    finally:
        connection.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--broker-socket', required=True, type=Path)
    parser.add_argument('--policy', required=True, type=Path)
    parser.add_argument('images', nargs=2, type=Path)
    args = parser.parse_args()
    _, _, raw_policy = oci.regular(args.policy, 65536, True)
    requested = json.loads(raw_policy, object_pairs_hook=oci.pairs)
    body, authority = read_contract(args.broker_socket)
    containers, architecture = policy(body, args.broker_socket, requested)
    images = oci.images(args.images, architecture)
    # A second active handshake prevents publishing inputs for a replaced Broker during hashing.
    oci.require(read_contract(args.broker_socket) == (body, authority), 'BROKER_CONTRACT_CHANGED')
    encoded = json.dumps({'containers': containers, 'images': images}, separators=(',', ':'))
    oci.require(len(encoded.encode()) <= 65536, 'INPUT_SIZE')
    print(encoded)


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        code = str(error) if isinstance(error, ValueError) else ''
        if re.fullmatch('[A-Z_]+', code) is None:
            code = type(error).__name__
        print('P23_INPUT_PREPARATION_FAILED: ' + code, file=sys.stderr)
        raise SystemExit(1)
