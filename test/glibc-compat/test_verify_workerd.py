"""Reject stale artifacts and tampered allocator evidence before packaging."""
import copy
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('verify_workerd', Path(__file__).with_name('verify-workerd.py'))
VERIFIER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VERIFIER)


class VerifyWorkerdTests(unittest.TestCase):
    def setUp(self):
        (ROOT / '.temp').mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix='allocator-evidence-', dir=ROOT / '.temp')
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.pin = json.loads((ROOT / 'packages/runtime/workerd-glibc228.lock.json').read_text())
        self.inputs = self.pin['source']['buildInputs']
        self.binary = b'allocator verifier fixture'
        archive = gzip.compress(self.binary, mtime=0)
        self.selected = self.pin['targets']['linux-x64']
        self.selected['binarySha256'] = hashlib.sha256(self.binary).hexdigest()
        self.selected['archiveSha256'] = hashlib.sha256(archive).hexdigest()
        self.metadata = {'id': int(self.inputs['compatibilityArtifactId']),
                         'name': 'workerd-linux-x64-glibc228-system-malloc-diagnostic',
                         'expired': False, 'workflow_run': {
                             'head_sha': self.inputs['compatibilityProducerCommit'],
                             'id': int(self.inputs['compatibilityWorkflowRun'].rsplit('/', 1)[1])}}
        (self.directory / self.selected['archiveName']).write_bytes(archive)
        (self.directory / 'workerd').write_bytes(self.binary)
        for name, value in {
            'producer-commit.txt': self.inputs['compatibilityProducerCommit'],
            'builder.sha256': self.inputs['compatibilityBuilderSha256'] + '  test/glibc-compat/Dockerfile',
            'allocator.txt': 'system',
            'allocator-bazel-option.txt': '--//src/workerd/server:use_tcmalloc=false',
            'source-revision.txt': self.pin['revision'],
            'version.txt': self.pin['expectedVersionOutput'],
            'protocol.txt': self.inputs['privateContainerProtocol'],
        }.items():
            (self.directory / name).write_text(value + '\n')
        catalog = b'catalog fixture'
        (self.directory / 'catalog.json').write_bytes(catalog)
        self.pin['compatibilityCatalog']['sha256'] = hashlib.sha256(catalog).hexdigest()
        self.allocator = {'passed': True, 'allocator': 'system',
                          'sha256': self.selected['binarySha256'],
                          'dynamicSymbols': {'malloc': 'SHN_UNDEF', 'calloc': 'SHN_UNDEF'}}
        self.smoke = {'passed': True, 'allocator': 'system', 'glibc': 'glibc 2.28',
                      'httpStatus': 200, 'response': 'glibc228-worker-ok'}
        self.write_json('allocator-symbols.json', self.allocator)
        self.write_json('centos82-smoke.json', self.smoke)

    def write_json(self, name, value):
        (self.directory / name).write_text(json.dumps(value))

    def verify(self):
        VERIFIER.verify_evidence(self.pin, self.metadata, self.directory,
                                 self.inputs['compatibilityBuilderSha256'])

    def test_verified_system_allocator_artifact_is_accepted(self):
        self.verify()

    def test_old_or_untrusted_artifact_metadata_is_rejected(self):
        for field, value in [('name', 'workerd-linux-x64-glibc228-experiment'),
                             ('expired', True), ('id', self.metadata['id'] + 1)]:
            with self.subTest(field=field):
                original = self.metadata[field]
                self.metadata[field] = value
                with self.assertRaises(AssertionError):
                    self.verify()
                self.metadata[field] = original
        self.metadata['workflow_run']['head_sha'] = '0' * 40
        with self.assertRaises(AssertionError):
            self.verify()

    def test_corrupt_archive_and_extracted_binary_are_rejected(self):
        archive = self.directory / self.selected['archiveName']
        original = archive.read_bytes()
        archive.write_bytes(original + b'corrupt')
        with self.assertRaises(AssertionError):
            self.verify()
        archive.write_bytes(original)
        (self.directory / 'workerd').write_bytes(b'changed')
        with self.assertRaises(AssertionError):
            self.verify()

    def test_allocator_reports_must_match_binary_and_import_libc(self):
        for field, value in [('passed', False), ('allocator', 'tcmalloc'),
                             ('sha256', '0' * 64),
                             ('dynamicSymbols', {'malloc': 1, 'calloc': 'SHN_UNDEF'}),
                             ('dynamicSymbols', {'malloc': 'SHN_UNDEF'})]:
            with self.subTest(field=field, value=value):
                report = copy.deepcopy(self.allocator)
                report[field] = value
                self.write_json('allocator-symbols.json', report)
                with self.assertRaises(AssertionError):
                    self.verify()

    def test_allocator_selection_and_smoke_must_use_system(self):
        for name, invalid in [('allocator.txt', 'tcmalloc'),
                              ('allocator-bazel-option.txt', '--//src/workerd/server:use_tcmalloc=true')]:
            with self.subTest(name=name):
                path = self.directory / name
                original = path.read_text()
                path.write_text(invalid)
                with self.assertRaises(AssertionError):
                    self.verify()
                path.write_text(original)
        self.smoke['allocator'] = 'tcmalloc'
        self.write_json('centos82-smoke.json', self.smoke)
        with self.assertRaises(AssertionError):
            self.verify()


if __name__ == '__main__':
    unittest.main()
