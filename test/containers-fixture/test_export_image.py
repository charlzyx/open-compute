"""Bounded Docker-save conversion and digest failure cases."""
import gzip
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('export_image', HERE / 'export-image.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Export(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name).resolve()
        self.source = self.root / 'image.tar'
        self.destination = self.root / 'oci'
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w') as archive:
            value = b'controlled container bytes'
            entry = tarfile.TarInfo('app/data')
            entry.size = len(value)
            archive.addfile(entry, io.BytesIO(value))
        self.layer = stream.getvalue()

    def tearDown(self):
        self.temporary.cleanup()

    def save(self, compressed=False, bad_diff=False, extra=None, duplicate=False, architecture='arm64'):
        diff = 'sha256:' + ('0' * 64 if bad_diff else hashlib.sha256(self.layer).hexdigest())
        config = json.dumps({'os': 'linux', 'architecture': architecture,
                             'rootfs': {'type': 'layers', 'diff_ids': [diff]}}).encode()
        name = hashlib.sha256(config).hexdigest() + '.json'
        manifest = json.dumps([{'Config': name, 'RepoTags': ['controlled:fixture'],
                                'Layers': ['layer.tar']}]).encode()
        with tarfile.open(self.source, 'w') as archive:
            rows = [('manifest.json', manifest), (name, config),
                    ('layer.tar', gzip.compress(self.layer, mtime=0) if compressed else self.layer)]
            if extra:
                rows.append(extra)
            if duplicate:
                rows.append(rows[0])
            for path, data in rows:
                entry = tarfile.TarInfo(path)
                entry.size = len(data)
                archive.addfile(entry, io.BytesIO(data))

    def test_raw_and_gzip_layers_preserve_diff_id_and_source_identity(self):
        for compressed in (False, True):
            with self.subTest(compressed=compressed):
                self.save(compressed=compressed)
                before = fixture.oci.identity(self.source.lstat())
                dest = self.root / str(compressed)
                result = fixture.export(self.source, dest, 'arm64')
                self.assertEqual(result['layers'], 1)
                self.assertEqual(result['unpackedBytes'], len(self.layer))
                self.assertEqual(before, fixture.oci.identity(self.source.lstat()))
                self.assertTrue(fixture.oci.image(dest, 'arm64'))
                manifest = json.loads((dest / 'manifest.json').read_bytes())
                self.assertEqual(manifest['layers'][0]['mediaType'],
                                 'application/vnd.oci.image.layer.v1.tar+gzip')

    def test_raw_layer_compression_is_reproducible(self):
        self.save()
        first = fixture.export(self.source, self.root / 'first', 'arm64')
        second = fixture.export(self.source, self.root / 'second', 'arm64')
        self.assertEqual(first, second)

    def test_wrong_diff_id_is_rejected_without_publishing_index(self):
        self.save(bad_diff=True)
        with self.assertRaisesRegex(ValueError, '^SAVE_DIFF_ID$'):
            fixture.export(self.source, self.destination, 'arm64')
        self.assertFalse((self.destination / 'index.json').exists())

    def test_duplicate_archive_member_is_rejected(self):
        self.save(duplicate=True)
        with self.assertRaisesRegex(ValueError, '^SAVE_ENTRY_TYPE_OR_DUPLICATE$'):
            fixture.export(self.source, self.destination, 'arm64')

    def test_escape_and_absolute_archive_names_are_rejected(self):
        for name in ('../outside', '/absolute', 'a/../outside', 'a\\outside'):
            with self.subTest(name=name):
                self.save(extra=(name, b'controlled'))
                with self.assertRaisesRegex(ValueError, '^SAVE_ENTRY_PATH$'):
                    fixture.export(self.source, self.destination, 'arm64')

    def test_wrong_platform_is_rejected(self):
        self.save(architecture='amd64')
        with self.assertRaisesRegex(ValueError, '^SAVE_IMAGE_PLATFORM$'):
            fixture.export(self.source, self.destination, 'arm64')

    def test_existing_output_is_not_reused(self):
        self.save()
        self.destination.mkdir()
        with self.assertRaisesRegex(ValueError, '^SAVE_CANONICAL_AND_FRESH_PATHS$'):
            fixture.export(self.source, self.destination, 'arm64')

    def test_unpacked_limit_applies_to_gzip(self):
        stream = io.BytesIO()
        with tarfile.open(fileobj=stream, mode='w') as archive:
            data = b'x' * (128 << 10)
            entry = tarfile.TarInfo('app/data')
            entry.size = len(data)
            archive.addfile(entry, io.BytesIO(data))
        self.layer = stream.getvalue()
        self.save(compressed=True)
        old = fixture.LIMIT
        try:
            fixture.LIMIT = 32 << 10
            self.assertLessEqual(self.source.stat().st_size, fixture.LIMIT)
            with self.assertRaisesRegex(ValueError, '^SAVE_UNPACKED_LIMIT$'):
                fixture.export(self.source, self.destination, 'arm64')
        finally:
            fixture.LIMIT = old

    def test_link_member_is_rejected_without_extraction(self):
        self.save()
        with tarfile.open(self.source, 'a') as archive:
            entry = tarfile.TarInfo('link')
            entry.type = tarfile.SYMTYPE
            entry.linkname = '../outside'
            archive.addfile(entry)
        with self.assertRaisesRegex(ValueError, '^SAVE_ENTRY_TYPE_OR_DUPLICATE$'):
            fixture.export(self.source, self.destination, 'arm64')
        self.assertFalse(self.destination.exists())


if __name__ == '__main__':
    unittest.main()
