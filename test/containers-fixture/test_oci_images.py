"""Small success and malformed-input checks for the disposable OCI projector."""
import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('oci_images', HERE / 'oci-images.py')
oci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oci)


def encode(value):
    return json.dumps(value, separators=(',', ':')).encode()


class Images(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name).resolve()
        self.blobs = self.root / 'blobs' / 'sha256'
        self.blobs.mkdir(parents=True)
        self.layer = self.put(b'controlled image layer', 'application/vnd.oci.image.layer.v1.tar')
        self.config_value = {'os': 'linux', 'architecture': 'arm64', 'rootfs': {
            'type': 'layers', 'diff_ids': [self.layer['digest']]}}
        self.config = self.put(encode(self.config_value), oci.CONFIG)
        self.manifest = {'schemaVersion': 2, 'mediaType': oci.MANIFEST,
                         'config': self.config, 'layers': [self.layer]}
        (self.root / 'oci-layout').write_bytes(encode({'imageLayoutVersion': '1.0.0'}))
        self.commit()

    def tearDown(self):
        self.temporary.cleanup()

    def put(self, raw, media):
        digest = hashlib.sha256(raw).hexdigest()
        (self.blobs / digest).write_bytes(raw)
        return {'mediaType': media, 'digest': 'sha256:' + digest, 'size': len(raw)}

    def commit(self):
        entry = self.put(encode(self.manifest), oci.MANIFEST)
        entry['platform'] = {'os': 'linux', 'architecture': 'arm64'}
        self.index = {'schemaVersion': 2, 'manifests': [entry]}
        (self.root / 'index.json').write_bytes(encode(self.index))

    def rejects(self, code):
        with self.assertRaisesRegex(ValueError, '^' + code + '$'):
            oci.image(self.root, 'arm64')

    def test_graph_projects_only_rust_consumer_fields(self):
        result = oci.image(self.root, 'arm64')
        self.assertEqual(set(result), {'manifest', 'blobs'})
        self.assertEqual(len(result['blobs']), 2)
        self.assertTrue(all(set(blob) == {'path', 'sha256'} for blob in result['blobs']))
        for blob in result['blobs']:
            self.assertEqual(hashlib.sha256(Path(blob['path']).read_bytes()).hexdigest(), blob['sha256'])

    def test_wrong_config_platform_is_rejected_even_when_index_matches(self):
        self.config_value['architecture'] = 'amd64'
        self.manifest['config'] = self.put(encode(self.config_value), oci.CONFIG)
        self.commit()
        self.rejects('OCI_CONFIG_PLATFORM')

    def test_multiple_index_manifests_are_rejected(self):
        self.index['manifests'] *= 2
        (self.root / 'index.json').write_bytes(encode(self.index))
        self.rejects('OCI_SINGLE_MANIFEST_REQUIRED')

    def test_duplicate_json_field_is_rejected(self):
        (self.root / 'oci-layout').write_bytes(b'{"imageLayoutVersion":"1.0.0","imageLayoutVersion":"1.0.0"}')
        self.rejects('JSON_DUPLICATE_FIELD')

    def test_layer_content_tamper_is_rejected(self):
        (self.blobs / self.layer['digest'][7:]).write_bytes(b'changed layer content')
        self.rejects('OCI_LAYER_DIGEST_OR_SIZE')

    def test_layer_size_tamper_is_rejected(self):
        self.manifest['layers'][0]['size'] += 1
        self.commit()
        self.rejects('OCI_LAYER_DIGEST_OR_SIZE')

    def test_symlink_blob_is_rejected(self):
        layer = self.blobs / self.layer['digest'][7:]
        target = self.root / 'outside'
        layer.rename(target)
        layer.symlink_to(target)
        self.rejects('NONCANONICAL_PATH')

    def test_path_digest_cannot_escape_layout(self):
        self.manifest['layers'][0]['digest'] = 'sha256:../outside'
        self.commit()
        self.rejects('DESCRIPTOR_DIGEST_OR_SIZE')

    def test_declared_oversize_graph_is_rejected_before_reading_layer(self):
        self.manifest['layers'][0]['size'] = oci.LIMIT
        self.commit()
        self.rejects('OCI_GRAPH_BUDGET')

    def test_valid_amd64_graph_is_supported(self):
        self.config_value['architecture'] = 'amd64'
        self.manifest['config'] = self.put(encode(self.config_value), oci.CONFIG)
        self.commit()
        self.index['manifests'][0]['platform']['architecture'] = 'amd64'
        (self.root / 'index.json').write_bytes(encode(self.index))
        self.assertEqual(len(oci.image(self.root, 'amd64')['blobs']), 2)

    def test_same_content_in_different_directories_is_not_a_distinct_image(self):
        with tempfile.TemporaryDirectory() as temporary:
            other = Path(temporary).resolve()
            shutil.copytree(self.root, other, dirs_exist_ok=True)
            with self.assertRaisesRegex(ValueError, '^DISTINCT_IMAGE_INPUTS_REQUIRED$'):
                oci.images([self.root, other], 'arm64')

    def test_projection_requires_exactly_two_images(self):
        for directories in ([], [self.root], [self.root] * 3):
            with self.subTest(count=len(directories)):
                with self.assertRaisesRegex(ValueError, '^TWO_IMAGE_INPUTS_REQUIRED$'):
                    oci.images(directories, 'arm64')


if __name__ == '__main__':
    unittest.main()
