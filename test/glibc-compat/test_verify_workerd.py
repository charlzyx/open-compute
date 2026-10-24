"""Reject stale artifacts and tampered allocator evidence before packaging."""
import copy
import gzip
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

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
        self.formal = json.loads((ROOT / 'packages/runtime/workerd.lock.json').read_text())
        self.pin = copy.deepcopy(self.formal)
        self.pin['release'] += '-system-malloc-glibc228'
        self.pin['targets'] = {'linux-x64': self.pin['targets']['linux-x64']}
        self.inputs = self.pin['source']['buildInputs']
        self.inputs.update({
            'originalRelease': self.formal['release'],
            'compatibilityProfile': 'linux-x64-glibc228',
            'compatibilityWorkflowRun': 'https://github.com/charlzyx/open-compute/actions/runs/101',
            'compatibilityArtifactId': '202',
            'compatibilityProducerCommit': 'a' * 40,
            'compatibilityBuilderSha256': hashlib.sha256(b'builder fixture').hexdigest(),
            'compatibilityBuildScriptSha256': hashlib.sha256(b'build script fixture').hexdigest(),
            'allocator': 'system',
            'allocatorBazelOption': '--//src/workerd/server:use_tcmalloc=false',
        })
        self.builder_sha256 = self.inputs['compatibilityBuilderSha256']
        self.build_script_sha256 = self.inputs['compatibilityBuildScriptSha256']
        self.binary = b'allocator verifier fixture'
        archive = gzip.compress(self.binary, mtime=0)
        self.selected = self.pin['targets']['linux-x64']
        self.selected['binarySha256'] = hashlib.sha256(self.binary).hexdigest()
        self.selected['archiveSha256'] = hashlib.sha256(archive).hexdigest()
        self.selected['archiveUrl'] = self.inputs['compatibilityWorkflowRun'] + '/artifacts/202'
        self.metadata = {'id': int(self.inputs['compatibilityArtifactId']),
                         'name': 'workerd-linux-x64-glibc228-system-malloc-diagnostic',
                         'expired': False, 'workflow_run': {
                             'head_sha': self.inputs['compatibilityProducerCommit'],
                             'id': int(self.inputs['compatibilityWorkflowRun'].rsplit('/', 1)[1])}}
        self.run = {'id': 101, 'html_url': self.inputs['compatibilityWorkflowRun'],
                    'repository': {'full_name': 'charlzyx/open-compute'},
                    'head_repository': {'full_name': 'charlzyx/open-compute'},
                    'head_sha': self.inputs['compatibilityProducerCommit'],
                    'path': '.github/workflows/centos8-system-malloc.yml',
                    'status': 'completed', 'conclusion': 'success'}
        (self.directory / self.selected['archiveName']).write_bytes(archive)
        (self.directory / 'workerd').write_bytes(self.binary)
        for name, value in {
            'producer-commit.txt': self.inputs['compatibilityProducerCommit'],
            'builder.sha256': self.inputs['compatibilityBuilderSha256'] + '  test/glibc-compat/Dockerfile',
            'build-script.sha256': self.inputs['compatibilityBuildScriptSha256'] + '  test/glibc-compat/build-workerd.sh',
            'io-backend.txt': 'rust-tokio',
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
        self.formal['compatibilityCatalog']['sha256'] = self.pin['compatibilityCatalog']['sha256']
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
        return VERIFIER.verify_evidence(self.pin, self.formal, self.metadata, self.run, self.directory,
                                        self.builder_sha256, self.build_script_sha256)

    def retain(self):
        self.verify()
        proof = (json.dumps({'schemaVersion': 1, 'pin': self.pin,
                             'run': self.run, 'artifact': self.metadata},
                            indent=2, sort_keys=True) + '\n').encode()
        self.write_proof(proof)
        self.inputs.update({
            'compatibilityBuildWorkflowRun': self.inputs['compatibilityWorkflowRun'],
            'compatibilityBuildArtifactId': self.inputs['compatibilityArtifactId'],
            'compatibilityBuildProducerCommit': self.inputs['compatibilityProducerCommit'],
            'compatibilityWorkflowRun': 'https://github.com/charlzyx/open-compute/actions/runs/303',
            'compatibilityArtifactId': '404',
            'compatibilityProducerCommit': 'd' * 40,
        })
        self.selected['archiveUrl'] = self.inputs['compatibilityWorkflowRun'] + '/artifacts/404'
        self.metadata.update({'id': 404, 'name': 'workerd-linux-x64-glibc228-qualified',
                              'workflow_run': {'id': 303, 'head_sha': 'd' * 40}})
        self.run.update({'id': 303, 'html_url': self.inputs['compatibilityWorkflowRun'],
                         'head_sha': 'd' * 40, 'path': '.github/workflows/centos8-retain-runtime.yml'})
        return proof

    def write_proof(self, proof):
        (self.directory / VERIFIER.PROOF_PATH).write_bytes(proof)
        self.inputs['compatibilityBuildProvenanceSha256'] = hashlib.sha256(proof).hexdigest()

    def verify_cli(self):
        checkout = self.directory / 'checkout'
        for path in ('packages/runtime', 'test/glibc-compat', '.temp/glibc-ocd'):
            (checkout / path).mkdir(parents=True)
        (checkout / '.temp/glibc-ocd/workerd').symlink_to(self.directory, target_is_directory=True)
        (checkout / 'packages/runtime/workerd.lock.json').write_text(json.dumps(self.formal))
        (checkout / 'packages/runtime/workerd-glibc228.lock.json').write_text(json.dumps(self.pin))
        (checkout / 'test/glibc-compat/Dockerfile').write_bytes(b'builder fixture')
        (checkout / 'test/glibc-compat/build-workerd.sh').write_bytes(b'build script fixture')
        # Deleted original GitHub metadata must not be queried after retention.
        responses = {
            f"repos/{VERIFIER.REPOSITORY}/actions/artifacts/{self.metadata['id']}": self.metadata,
            f"repos/{VERIFIER.REPOSITORY}/actions/runs/{self.run['id']}": self.run,
        }
        with mock.patch.object(VERIFIER, 'ROOT', checkout), mock.patch.object(
                VERIFIER.subprocess, 'check_output',
                side_effect=lambda command, **kwargs: json.dumps(responses[command[-1]])) as api, \
                mock.patch('builtins.print') as output:
            VERIFIER.verify()
        self.assertEqual(api.call_args_list, [
            mock.call(['gh', 'api', path], text=True) for path in responses])
        return json.loads(output.call_args.args[0])

    def test_verified_system_allocator_artifact_is_accepted(self):
        self.verify()

    def test_retained_copy_preserves_original_native_build_identity(self):
        self.retain()
        self.assertNotEqual(self.inputs['compatibilityProducerCommit'],
                            self.inputs['compatibilityBuildProducerCommit'])
        self.verify()
        (self.directory / 'producer-commit.txt').write_text(self.inputs['compatibilityProducerCommit'])
        with self.assertRaises(AssertionError):
            self.verify()

    def test_retained_cli_uses_only_current_transport_and_preserves_exact_proof_bytes(self):
        evidence = json.loads(self.retain())
        proof = json.dumps(evidence, separators=(',', ':')).encode() + b'\n\n'
        self.write_proof(proof)
        output = self.verify_cli()
        self.assertEqual((self.directory / VERIFIER.PROOF_PATH).read_bytes(), proof)
        self.assertEqual(output['producerCommit'], 'd' * 40)
        self.assertEqual(output['buildProducerCommit'], 'a' * 40)

    def test_original_cli_does_not_create_a_proof(self):
        output = self.verify_cli()
        self.assertFalse((self.directory / VERIFIER.PROOF_PATH).exists())
        self.assertEqual(output['buildProducerCommit'], 'a' * 40)

    def test_later_retention_keeps_the_same_original_proof(self):
        proof = self.retain()
        self.verify()
        self.inputs.update({
            'compatibilityWorkflowRun': 'https://github.com/charlzyx/open-compute/actions/runs/505',
            'compatibilityArtifactId': '606', 'compatibilityProducerCommit': 'e' * 40,
        })
        self.selected['archiveUrl'] = self.inputs['compatibilityWorkflowRun'] + '/artifacts/606'
        self.metadata.update({'id': 606, 'workflow_run': {'id': 505, 'head_sha': 'e' * 40}})
        self.run.update({'id': 505, 'html_url': self.inputs['compatibilityWorkflowRun'],
                         'head_sha': 'e' * 40})
        self.verify()
        self.assertEqual((self.directory / VERIFIER.PROOF_PATH).read_bytes(), proof)

    def test_retention_requires_all_provenance_fields_as_nonempty_strings(self):
        self.retain()
        for field in VERIFIER.PROVENANCE_FIELDS:
            original = self.inputs[field]
            for value in (None, '', 123):
                with self.subTest(field=field, value=value):
                    self.inputs[field] = value
                    with self.assertRaises(AssertionError):
                        self.verify()
            del self.inputs[field]
            with self.subTest(field=field, missing=True), self.assertRaises(AssertionError):
                self.verify()
            self.inputs[field] = original

    def test_retained_original_identity_fields_must_match_proof(self):
        self.retain()
        for field in VERIFIER.BUILD_IDENTITY_FIELDS:
            with self.subTest(field=field):
                original = self.inputs[field]
                self.inputs[field] += '0'
                with self.assertRaises(AssertionError):
                    self.verify()
                self.inputs[field] = original

    def test_proof_tampering_missing_proof_and_unknown_schema_are_rejected(self):
        proof = self.retain()
        path = self.directory / VERIFIER.PROOF_PATH
        path.write_bytes(proof + b' ')
        with self.assertRaises(AssertionError):
            self.verify()
        for version in (2, True):
            evidence = json.loads(proof)
            evidence['schemaVersion'] = version
            self.write_proof(json.dumps(evidence).encode())
            with self.subTest(version=version), self.assertRaises(AssertionError):
                self.verify()
        self.write_proof(proof)
        path.unlink()
        with self.assertRaises(FileNotFoundError):
            self.verify()

    def test_proof_cannot_replace_the_original_pin_with_an_archived_pin(self):
        proof = self.retain()
        evidence = json.loads(proof)
        evidence['pin'] = copy.deepcopy(self.pin)
        self.write_proof(json.dumps(evidence).encode())
        with self.assertRaisesRegex(AssertionError, 'original native build'):
            self.verify()

    def test_retention_cannot_change_any_original_build_or_binary_contract(self):
        self.retain()
        for mapping, field, value in [
            (self.inputs, 'compiler', 'changed compiler'),
            (self.inputs, 'libc', 'glibc 2.31'),
            (self.inputs, 'qualification', 'not the original qualification'),
            (self.selected, 'archiveName', 'renamed.gz'),
            (self.selected, 'archiveSha256', '0' * 64),
            (self.selected, 'binarySha256', '0' * 64),
        ]:
            with self.subTest(field=field):
                original = mapping.get(field)
                existed = field in mapping
                mapping[field] = value
                with self.assertRaisesRegex(AssertionError, 'original binary and build contract'):
                    self.verify()
                if existed:
                    mapping[field] = original
                else:
                    del mapping[field]
        self.pin['unexpected'] = 'added metadata'
        with self.assertRaisesRegex(AssertionError, 'original binary and build contract'):
            self.verify()

    def test_retained_current_transport_must_be_successful_unexpired_and_exact(self):
        self.retain()
        for mapping, field, value in [
            (self.metadata, 'id', 405), (self.metadata, 'expired', True),
            (self.metadata, 'name', VERIFIER.ARTIFACT_NAME),
            (self.metadata['workflow_run'], 'id', 304),
            (self.metadata['workflow_run'], 'head_sha', '0' * 40),
            (self.run, 'id', 304), (self.run, 'path', VERIFIER.WORKFLOW),
            (self.run, 'html_url', 'https://github.com/other/repo/actions/runs/303'),
            (self.run, 'repository', {'full_name': 'other/repo'}),
            (self.run, 'head_repository', {'full_name': 'other/repo'}),
            (self.run, 'head_sha', '0' * 40), (self.run, 'status', 'in_progress'),
            (self.run, 'conclusion', 'failure'),
            (self.selected, 'archiveUrl', self.selected['archiveUrl'] + '0'),
        ]:
            with self.subTest(field=field, value=value):
                original = mapping[field]
                mapping[field] = value
                with self.assertRaises(AssertionError):
                    self.verify()
                mapping[field] = original

    def test_pinned_proof_still_requires_a_successful_original_native_build(self):
        proof = self.retain()
        for section, field, value in [
            ('artifact', 'id', 203), ('artifact', 'expired', True),
            ('artifact', 'name', VERIFIER.RETAIN_ARTIFACT_NAME),
            ('run', 'id', 102), ('run', 'path', VERIFIER.RETAIN_WORKFLOW),
            ('run', 'html_url', 'https://github.com/other/repo/actions/runs/101'),
            ('run', 'repository', {'full_name': 'other/repo'}),
            ('run', 'head_repository', {'full_name': 'other/repo'}),
            ('run', 'head_sha', '0' * 40), ('run', 'status', 'in_progress'),
            ('run', 'conclusion', 'failure'),
        ]:
            with self.subTest(section=section, field=field, value=value):
                evidence = json.loads(proof)
                evidence[section][field] = value
                self.write_proof(json.dumps(evidence).encode())
                with self.assertRaises(AssertionError):
                    self.verify()

    def test_retained_files_must_still_match_the_original_verified_evidence(self):
        self.retain()
        for name, value in [
            ('workerd', b'changed binary'),
            (self.selected['archiveName'], b'changed archive'),
            ('producer-commit.txt', b'changed producer'),
            ('io-backend.txt', b'cxx'), ('catalog.json', b'changed catalog'),
        ]:
            with self.subTest(name=name):
                path = self.directory / name
                original = path.read_bytes()
                path.write_bytes(value)
                with self.assertRaises(AssertionError):
                    self.verify()
                path.write_bytes(original)

    def test_old_revision_and_changed_formal_metadata_are_rejected(self):
        for field, value in [
            ('schemaVersion', 3), ('revision', '0' * 40),
            ('expectedVersionOutput', 'workerd 2000-01-01'),
            ('systemCompatibilityDate', '2000-01-01'),
            ('systemCompatibilityFlags', []), ('binaryMaximumCompatibilityDate', '2000-01-01'),
            ('compatibilityCatalog', {'schemaVersion': 1, 'sha256': '0' * 64}),
            ('processFlags', []), ('pyodideBundle', {}), ('workersTypes', {}), ('workersSdk', {}),
            ('release', self.pin['release'] + '-changed'),
        ]:
            with self.subTest(field=field):
                original = self.pin[field]
                self.pin[field] = value
                with self.assertRaises(AssertionError):
                    self.verify()
                self.pin[field] = original

    def test_changed_source_and_build_contracts_are_rejected(self):
        for mapping, field, value in [
            (self.pin['source'], 'repository', 'https://github.com/other/workerd'),
            (self.pin['source'], 'upstreamBase', '0' * 40),
            (self.inputs, 'originalRelease', 'old-release'),
            (self.inputs, 'compatibilityProfile', 'unknown'),
            (self.inputs, 'privateContainerProtocol', 'open-compute-container-protocol-v3'),
            (self.inputs, 'bazel', '0.0.0'), (self.inputs, 'target', '//other:workerd'),
            (self.inputs, 'mode', 'fastbuild'), (self.inputs, 'strip', 'never'),
            (self.inputs, 'pyodideBundleTarget', '//other:bundle'),
            (self.inputs, 'pyodideBundleArchive', 'unverified'),
        ]:
            with self.subTest(field=field):
                original = mapping[field]
                mapping[field] = value
                with self.assertRaises(AssertionError):
                    self.verify()
                mapping[field] = original
        self.pin['targets']['linux-arm64'] = copy.deepcopy(self.selected)
        with self.assertRaises(AssertionError):
            self.verify()

    def test_producer_run_must_be_successful_and_belong_to_the_fork_workflow(self):
        for field, value in [
            ('id', 102), ('html_url', 'https://github.com/other/open-compute/actions/runs/101'),
            ('repository', {'full_name': 'elliothux/open-compute'}),
            ('head_repository', {'full_name': 'other/open-compute'}),
            ('head_sha', '0' * 40), ('path', '.github/workflows/centos8-compat.yml'),
            ('status', 'in_progress'), ('conclusion', 'failure'),
            ('conclusion', 'cancelled'), ('conclusion', None),
        ]:
            with self.subTest(field=field, value=value):
                original = self.run[field]
                self.run[field] = value
                with self.assertRaises(AssertionError):
                    self.verify()
                self.run[field] = original

    def test_workflow_and_archive_urls_must_identify_the_exact_fork_artifact(self):
        for mapping, field, value in [
            (self.inputs, 'compatibilityWorkflowRun', 'https://github.com/other/repo/actions/runs/101'),
            (self.selected, 'archiveUrl', self.selected['archiveUrl'] + '0'),
        ]:
            with self.subTest(field=field):
                original = mapping[field]
                mapping[field] = value
                with self.assertRaises(AssertionError):
                    self.verify()
                mapping[field] = original

    def test_builder_script_and_io_backend_must_match(self):
        for field in ('compatibilityBuilderSha256', 'compatibilityBuildScriptSha256'):
            with self.subTest(field=field):
                original = self.inputs[field]
                self.inputs[field] = '0' * 64
                with self.assertRaises(AssertionError):
                    self.verify()
                self.inputs[field] = original
        for name, value in [('builder.sha256', '0' * 64),
                            ('build-script.sha256', '0' * 64), ('io-backend.txt', 'rust')]:
            with self.subTest(name=name):
                path = self.directory / name
                original = path.read_text()
                path.write_text(value)
                with self.assertRaises(AssertionError):
                    self.verify()
                path.write_text(original)

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
