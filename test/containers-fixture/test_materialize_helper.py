"""Bounded helper archives, ownership, readback and exact failure cleanup."""
import copy
import hashlib
import importlib.util
import io
from pathlib import Path
import tarfile
import unittest

spec = importlib.util.spec_from_file_location('materialize_helper', Path(__file__).with_name('materialize-helper.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
PAYLOAD = b'controlled helper executable'
DIGEST = hashlib.sha256(PAYLOAD).hexdigest()
IMAGE = 'sha256:' + 'a' * 64
VOLUME = 'controlled-new-helper'


def archive(name='helper', mode=0o555, kind=tarfile.REGTYPE, payload=PAYLOAD):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode='w:') as target:
        row = tarfile.TarInfo(name)
        row.mode, row.type = mode, kind
        row.size = len(payload) if kind == tarfile.REGTYPE else 0
        row.linkname = '/controlled/not-followed' if kind == tarfile.SYMTYPE else ''
        target.addfile(row, io.BytesIO(payload) if row.size else None)
    return output.getvalue()


class ControlledEngine:
    def __init__(self):
        self.calls = []
        self.containers = []
        self.volumes = {}
        self.readback_bad = False
        self.foreign_after = False
        self.inspections = 0
        self.architecture = 'arm64'
        self.sequence = 0

    def inventory(self):
        self.inspections += 1
        return {'containers': sorted(self.containers + (['foreign'] if self.foreign_after and self.inspections > 1 else [])),
                'images': [IMAGE], 'networks': ['controlled-network'], 'volumes': sorted(self.volumes)}

    def unchanged(self):
        pass

    def request(self, method, path, body=None, archive=False):
        self.calls.append((method, path))
        if path == f'/images/{IMAGE}/json':
            return 200, {'Id': IMAGE, 'Os': 'linux', 'Architecture': self.architecture, 'Config': {}}
        if method == 'POST' and path.startswith('/containers/create?name='):
            assert body['Image'] == IMAGE and body['HostConfig']['NetworkMode'] == 'none'
            self.sequence += 1
            identity = str(self.sequence) * 64
            self.containers.append(identity)
            return 201, {'Id': identity}
        if path == '/volumes/create':
            self.volumes[body['Name']] = {**copy.deepcopy(body), 'Scope': 'local', 'Options': None}
            return 201, self.volumes[body['Name']]
        if path.startswith('/volumes/'):
            name = path.removeprefix('/volumes/')
            if method == 'DELETE':
                del self.volumes[name]
                return 204, None
            return (200, self.volumes[name]) if name in self.volumes else (404, {})
        if '/archive?' in path:
            if method == 'PUT':
                assert body == globals()['archive']()
                return 200, None
            name = 'proxy-everything' if path.endswith('/proxy-everything') else 'helper'
            bad = self.readback_bad and path.startswith('/containers/' + '2' * 64)
            return 200, globals()['archive'](name=name, payload=b'wrong' if bad else PAYLOAD)
        if path.startswith('/containers/'):
            identity = path.split('/')[2]
            if method == 'DELETE':
                self.containers.remove(identity)
                return 204, None
            if path.endswith('/json'):
                return (200, {}) if identity in self.containers else (404, {})
        raise AssertionError((method, path))


class HelperMaterialization(unittest.TestCase):
    def test_exact_bounded_executable_archive(self):
        self.assertEqual(fixture.verify_archive(archive(), DIGEST), len(PAYLOAD))

    def test_symlink_is_never_followed(self):
        with self.assertRaisesRegex(ValueError, '^HELPER_ARCHIVE_TYPE$'):
            fixture.verify_archive(archive(kind=tarfile.SYMTYPE), DIGEST)

    def test_path_escape_is_rejected(self):
        with self.assertRaisesRegex(ValueError, '^HELPER_ARCHIVE_TYPE$'):
            fixture.verify_archive(archive(name='../helper'), DIGEST)

    def test_privileged_or_nonexecutable_mode_is_rejected(self):
        for mode in (0o4755, 0o644):
            with self.subTest(mode=mode), self.assertRaisesRegex(ValueError, '^HELPER_ARCHIVE_MODE$'):
                fixture.verify_archive(archive(mode=mode), DIGEST)

    def test_digest_mismatch_is_rejected(self):
        with self.assertRaisesRegex(ValueError, '^HELPER_ARCHIVE_DIGEST$'):
            fixture.verify_archive(archive(payload=b'wrong'), DIGEST)

    def test_zero_or_oversized_helper_is_rejected(self):
        with self.assertRaisesRegex(ValueError, '^HELPER_ARCHIVE_TYPE$'):
            fixture.verify_archive(archive(payload=b''), DIGEST)
        with self.assertRaisesRegex(ValueError, '^HELPER_ARCHIVE_LIMIT$'):
            fixture.verify_archive(b'x' * (fixture.LIMIT + 65537), DIGEST)

    def test_success_retains_only_the_verified_new_volume(self):
        engine = ControlledEngine()
        result = fixture.materialize(engine, IMAGE, DIGEST, VOLUME, 'arm64')
        self.assertTrue(result['success'])
        self.assertTrue(result['allOwnedContainersAbsent'])
        self.assertTrue(result['volumeRetained'])
        self.assertEqual(result['helperBytes'], len(PAYLOAD))
        self.assertFalse(result['providerQualified'])
        self.assertEqual(engine.containers, [])
        self.assertEqual(list(engine.volumes), [VOLUME])
        self.assertFalse(any('/start' in path or 'force=true' in path for _, path in engine.calls))

    def test_existing_volume_is_never_adopted_or_deleted(self):
        engine = ControlledEngine()
        engine.volumes[VOLUME] = {'unrelated': True}
        with self.assertRaisesRegex(ValueError, '^EMPTY_ENGINE_AND_NEW_VOLUME_REQUIRED$'):
            fixture.materialize(engine, IMAGE, DIGEST, VOLUME, 'arm64')
        self.assertEqual(engine.calls, [])
        self.assertEqual(engine.volumes[VOLUME], {'unrelated': True})

    def test_busy_engine_is_rejected_before_any_mutation(self):
        engine = ControlledEngine()
        engine.containers.append('foreign')
        with self.assertRaisesRegex(ValueError, '^EMPTY_ENGINE_AND_NEW_VOLUME_REQUIRED$'):
            fixture.materialize(engine, IMAGE, DIGEST, VOLUME, 'arm64')
        self.assertEqual(engine.calls, [])

    def test_wrong_architecture_is_rejected_before_create(self):
        engine = ControlledEngine()
        engine.architecture = 'amd64'
        with self.assertRaisesRegex(ValueError, '^SIDECAR_IMAGE_IDENTITY$'):
            fixture.materialize(engine, IMAGE, DIGEST, VOLUME, 'arm64')
        self.assertTrue(all(method == 'GET' for method, _ in engine.calls))

    def test_readback_failure_removes_only_owned_new_objects(self):
        engine = ControlledEngine()
        engine.readback_bad = True
        result = fixture.materialize(engine, IMAGE, DIGEST, VOLUME, 'arm64')
        self.assertFalse(result['success'])
        self.assertEqual(result['failureCode'], 'HELPER_ARCHIVE_DIGEST')
        self.assertEqual(engine.containers, [])
        self.assertEqual(engine.volumes, {})
        self.assertFalse(result['volumeRetained'])

    def test_foreign_concurrent_object_fails_inventory_without_deleting_it(self):
        engine = ControlledEngine()
        engine.foreign_after = True
        result = fixture.materialize(engine, IMAGE, DIGEST, VOLUME, 'arm64')
        self.assertFalse(result['success'])
        self.assertEqual(result['cleanupFailureCode'], 'HELPER_MATERIALIZATION_INVENTORY_CHANGED')
        self.assertFalse(any('foreign' in path and method == 'DELETE' for method, path in engine.calls))


if __name__ == '__main__':
    unittest.main()
