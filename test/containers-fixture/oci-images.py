#!/usr/bin/env python3
"""Validate two existing OCI layouts and emit the P23 image-input projection."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat

MANIFEST = 'application/vnd.oci.image.manifest.v1+json'
CONFIG = 'application/vnd.oci.image.config.v1+json'
LAYERS = {'application/vnd.oci.image.layer.v1.tar',
          'application/vnd.oci.image.layer.v1.tar+gzip',
          'application/vnd.oci.image.layer.v1.tar+zstd'}
LIMIT = 1 << 30
FIELDS = ('st_dev', 'st_ino', 'st_mode', 'st_nlink', 'st_uid', 'st_gid',
          'st_size', 'st_mtime_ns', 'st_ctime_ns')


def require(condition, code):
    if not condition:
        raise ValueError(code)


def pairs(rows):
    value = {}
    for key, item in rows:
        require(key not in value, 'JSON_DUPLICATE_FIELD')
        value[key] = item
    return value


def identity(value):
    return tuple(getattr(value, key) for key in FIELDS)


def regular(path, limit, contents=False):
    require(path.is_absolute() and path.resolve() == path, 'NONCANONICAL_PATH')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as source:
        before = os.fstat(source.fileno())
        require(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= limit,
                'FILE_TYPE_OR_SIZE')
        digest = hashlib.sha256()
        chunks = []
        count = 0
        while chunk := source.read(1 << 20):
            count += len(chunk)
            require(count <= limit, 'FILE_GREW')
            digest.update(chunk)
            if contents:
                chunks.append(chunk)
        require(count == before.st_size and identity(before) == identity(os.fstat(source.fileno()))
                and identity(before) == identity(path.lstat()), 'FILE_CHANGED')
    return digest.hexdigest(), count, b''.join(chunks) if contents else None


def document(path):
    digest, size, raw = regular(path, 1 << 20, True)
    value = json.loads(raw, object_pairs_hook=pairs)
    require(isinstance(value, dict), 'JSON_OBJECT_REQUIRED')
    return value, digest, size


def descriptor(value, media_types):
    require(isinstance(value, dict) and value.get('mediaType') in media_types,
            'DESCRIPTOR_MEDIA_TYPE')
    digest, size = value.get('digest'), value.get('size')
    require(isinstance(digest, str) and re.fullmatch(r'sha256:[0-9a-f]{64}', digest)
            and type(size) is int and 0 < size <= LIMIT, 'DESCRIPTOR_DIGEST_OR_SIZE')
    return digest[7:], size


def image(directory, architecture):
    directory = Path(directory)
    require(directory.is_absolute() and directory.resolve() == directory
            and directory.is_dir(), 'OCI_DIRECTORY')
    layout, _, _ = document(directory / 'oci-layout')
    require(layout == {'imageLayoutVersion': '1.0.0'}, 'OCI_LAYOUT_VERSION')
    index, _, _ = document(directory / 'index.json')
    require(index.get('schemaVersion') == 2 and isinstance(index.get('manifests'), list)
            and len(index['manifests']) == 1, 'OCI_SINGLE_MANIFEST_REQUIRED')
    entry = index['manifests'][0]
    expected, expected_size = descriptor(entry, {MANIFEST})
    platform = entry.get('platform')
    require(isinstance(platform, dict) and platform.get('os') == 'linux'
            and platform.get('architecture') == architecture, 'OCI_INDEX_PLATFORM')
    manifest_path = directory / 'blobs' / 'sha256' / expected
    manifest, digest, size = document(manifest_path)
    require((digest, size) == (expected, expected_size), 'OCI_MANIFEST_DIGEST_OR_SIZE')
    require(manifest.get('schemaVersion') == 2 and manifest.get('mediaType') == MANIFEST,
            'OCI_MANIFEST_SCHEMA')
    layers = manifest.get('layers')
    require(isinstance(layers, list) and 1 <= len(layers) <= 128, 'OCI_LAYER_COUNT')
    config_digest, config_size = descriptor(manifest.get('config'), {CONFIG})
    config_path = directory / 'blobs' / 'sha256' / config_digest
    config, actual, length = document(config_path)
    require((actual, length) == (config_digest, config_size), 'OCI_CONFIG_DIGEST_OR_SIZE')
    require(config.get('os') == 'linux' and config.get('architecture') == architecture,
            'OCI_CONFIG_PLATFORM')
    rootfs = config.get('rootfs')
    require(isinstance(rootfs, dict) and rootfs.get('type') == 'layers'
            and isinstance(rootfs.get('diff_ids'), list)
            and len(rootfs['diff_ids']) == len(layers)
            and all(isinstance(row, str) and re.fullmatch(r'sha256:[0-9a-f]{64}', row)
                    for row in rootfs['diff_ids']), 'OCI_ROOTFS')
    blobs = [{'path': str(config_path), 'sha256': config_digest}]
    total = size + config_size
    for row in layers:
        layer_digest, layer_size = descriptor(row, LAYERS)
        total += layer_size
        require(total <= LIMIT, 'OCI_GRAPH_BUDGET')
        path = directory / 'blobs' / 'sha256' / layer_digest
        actual, length, _ = regular(path, LIMIT)
        require((actual, length) == (layer_digest, layer_size), 'OCI_LAYER_DIGEST_OR_SIZE')
        blobs.append({'path': str(path), 'sha256': layer_digest})
    return {'manifest': str(manifest_path), 'blobs': blobs}


def images(directories, architecture):
    require(len(directories) == 2, 'TWO_IMAGE_INPUTS_REQUIRED')
    values = [image(path, architecture) for path in directories]
    require(Path(values[0]['manifest']).name != Path(values[1]['manifest']).name,
            'DISTINCT_IMAGE_INPUTS_REQUIRED')
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--architecture', required=True, choices=('arm64', 'amd64'))
    parser.add_argument('images', nargs=2, type=Path)
    args = parser.parse_args()
    print(json.dumps({'images': images(args.images, args.architecture)}, separators=(',', ':')))


if __name__ == '__main__':
    main()
