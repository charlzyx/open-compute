"""Bind the reused GitHub artifact to the tracked compatibility pin and evidence."""
import copy
import hashlib
import gzip
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = 'charlzyx/open-compute'
WORKFLOW = '.github/workflows/centos8-system-malloc.yml'
RETAIN_WORKFLOW = '.github/workflows/centos8-retain-runtime.yml'
ARTIFACT_NAME = 'workerd-linux-x64-glibc228-system-malloc-diagnostic'
RETAIN_ARTIFACT_NAME = 'workerd-linux-x64-glibc228-qualified'
PROOF_PATH = Path('build-provenance.json')
BUILD_IDENTITY_FIELDS = {
    'compatibilityBuildWorkflowRun': 'compatibilityWorkflowRun',
    'compatibilityBuildArtifactId': 'compatibilityArtifactId',
    'compatibilityBuildProducerCommit': 'compatibilityProducerCommit',
}
PROVENANCE_FIELDS = {*BUILD_IDENTITY_FIELDS, 'compatibilityBuildProvenanceSha256'}


def verify():
    pin = json.loads((ROOT / 'packages/runtime/workerd-glibc228.lock.json').read_text())
    formal = json.loads((ROOT / 'packages/runtime/workerd.lock.json').read_text())
    verify_formal_pin(pin, formal)
    inputs = pin['source']['buildInputs']
    artifact_id = inputs['compatibilityArtifactId']
    metadata = json.loads(subprocess.check_output([
        'gh', 'api', f'repos/{REPOSITORY}/actions/artifacts/{artifact_id}'], text=True))
    run_id = metadata['workflow_run']['id']
    run = json.loads(subprocess.check_output([
        'gh', 'api', f'repos/{REPOSITORY}/actions/runs/{run_id}'], text=True))
    directory = ROOT / '.temp/glibc-ocd/workerd'
    builder_sha256 = hashlib.sha256((ROOT / 'test/glibc-compat/Dockerfile').read_bytes()).hexdigest()
    build_script_sha256 = hashlib.sha256((ROOT / 'test/glibc-compat/build-workerd.sh').read_bytes()).hexdigest()
    verify_evidence(pin, formal, metadata, run, directory, builder_sha256, build_script_sha256)
    print(json.dumps({'artifact': metadata['id'], 'producerCommit': metadata['workflow_run']['head_sha'],
                      'buildProducerCommit': inputs.get('compatibilityBuildProducerCommit',
                                                        inputs['compatibilityProducerCommit']),
                      'archiveSha256': pin['targets']['linux-x64']['archiveSha256'],
                      'binarySha256': pin['targets']['linux-x64']['binarySha256'],
                      'allocator': 'system', 'passed': True}))


def verify_formal_pin(pin, formal):
    for field in ('schemaVersion', 'revision', 'expectedVersionOutput',
                  'systemCompatibilityDate', 'systemCompatibilityFlags',
                  'binaryMaximumCompatibilityDate', 'compatibilityCatalog',
                  'processFlags', 'pyodideBundle', 'workersTypes', 'workersSdk'):
        assert pin[field] == formal[field], f'Compatibility pin differs from the formal pin: {field}'
    for field in ('repository', 'upstreamBase'):
        assert pin['source'][field] == formal['source'][field], f'Compatibility source differs: {field}'
    inputs = pin['source']['buildInputs']
    assert inputs['originalRelease'] == formal['release']
    assert pin['release'] == formal['release'] + '-system-malloc-glibc228'
    assert inputs['compatibilityProfile'] == 'linux-x64-glibc228'
    assert set(pin['targets']) == {'linux-x64'}
    for field in ('bazel', 'target', 'mode', 'strip', 'privateContainerProtocol',
                  'pyodideBundleTarget', 'pyodideBundleArchive'):
        assert inputs[field] == formal['source']['buildInputs'][field], f'Compatibility build input differs: {field}'


def verify_evidence(pin, formal, metadata, run, directory, builder_sha256, build_script_sha256):
    verify_formal_pin(pin, formal)
    retained = is_retained(pin)
    verify_transport(pin, metadata, run, retained)
    if retained:
        # The pinned snapshot outlives the original GitHub run and artifact metadata.
        proof = (directory / PROOF_PATH).read_bytes()
        inputs = pin['source']['buildInputs']
        assert hashlib.sha256(proof).hexdigest() == inputs['compatibilityBuildProvenanceSha256']
        evidence = json.loads(proof)
        assert set(evidence) == {'schemaVersion', 'pin', 'run', 'artifact'}
        assert type(evidence['schemaVersion']) is int and evidence['schemaVersion'] == 1
        build_pin = evidence['pin']
        assert not is_retained(build_pin), 'Build provenance must identify the original native build'
        verify_retained_pin(pin, build_pin)
        verify_formal_pin(build_pin, formal)
        verify_transport(build_pin, evidence['artifact'], evidence['run'], False)
    else:
        build_pin = pin
    verify_build_evidence(build_pin, directory, builder_sha256, build_script_sha256)


def is_retained(pin):
    inputs = pin['source']['buildInputs']
    present = PROVENANCE_FIELDS.intersection(inputs)
    assert not present or present == PROVENANCE_FIELDS, 'Incomplete retained-build provenance'
    assert all(isinstance(inputs[field], str) and inputs[field] for field in present)
    return bool(present)


def verify_retained_pin(pin, build_pin):
    inputs = pin['source']['buildInputs']
    build_inputs = build_pin['source']['buildInputs']
    expected = copy.deepcopy(build_pin)
    expected_inputs = expected['source']['buildInputs']
    for build_field, transport_field in BUILD_IDENTITY_FIELDS.items():
        assert inputs[build_field] == build_inputs[transport_field], f'Original build differs: {build_field}'
        expected_inputs[transport_field] = inputs[transport_field]
    for field in PROVENANCE_FIELDS:
        expected_inputs[field] = inputs[field]
    expected['targets']['linux-x64']['archiveUrl'] = pin['targets']['linux-x64']['archiveUrl']
    assert pin == expected, 'Retention must preserve the original binary and build contract'


def verify_transport(pin, metadata, run, retained):
    inputs = pin['source']['buildInputs']
    artifact_id = inputs['compatibilityArtifactId']
    assert metadata['id'] == int(artifact_id)
    assert metadata['name'] == (RETAIN_ARTIFACT_NAME if retained else ARTIFACT_NAME)
    assert metadata['expired'] is False
    assert metadata['workflow_run']['head_sha'] == inputs['compatibilityProducerCommit']
    run_id = metadata['workflow_run']['id']
    run_url = f'https://github.com/{REPOSITORY}/actions/runs/{run_id}'
    assert inputs['compatibilityWorkflowRun'] == run_url
    assert run['id'] == run_id and run['html_url'] == run_url
    assert run['repository']['full_name'] == REPOSITORY
    assert run['head_repository']['full_name'] == REPOSITORY
    assert run['head_sha'] == inputs['compatibilityProducerCommit']
    assert run['path'] == (RETAIN_WORKFLOW if retained else WORKFLOW)
    assert run['status'] == 'completed' and run['conclusion'] == 'success'
    assert pin['targets']['linux-x64']['archiveUrl'] == f'{run_url}/artifacts/{artifact_id}'


def verify_build_evidence(pin, directory, builder_sha256, build_script_sha256):
    inputs = pin['source']['buildInputs']
    assert builder_sha256 == inputs['compatibilityBuilderSha256']
    assert build_script_sha256 == inputs['compatibilityBuildScriptSha256']
    assert (directory / 'producer-commit.txt').read_text().strip() == inputs['compatibilityProducerCommit']
    assert (directory / 'builder.sha256').read_text().split()[0] == builder_sha256
    assert (directory / 'build-script.sha256').read_text().split()[0] == build_script_sha256
    assert (directory / 'io-backend.txt').read_text().strip() == 'rust-tokio'
    selected = pin['targets']['linux-x64']
    archive = (directory / selected['archiveName']).read_bytes()
    assert len(archive) <= 64 * 1024 * 1024
    assert hashlib.sha256(archive).hexdigest() == selected['archiveSha256']
    binary = gzip.decompress(archive)
    assert len(binary) <= 256 * 1024 * 1024
    assert hashlib.sha256(binary).hexdigest() == selected['binarySha256']
    assert hashlib.sha256((directory / 'workerd').read_bytes()).hexdigest() == selected['binarySha256']
    assert inputs['allocator'] == 'system'
    assert inputs['allocatorBazelOption'] == '--//src/workerd/server:use_tcmalloc=false'
    assert (directory / 'allocator.txt').read_text().strip() == inputs['allocator']
    assert (directory / 'allocator-bazel-option.txt').read_text().strip() == inputs['allocatorBazelOption']
    allocator = json.loads((directory / 'allocator-symbols.json').read_text())
    assert allocator['passed'] is True and allocator['allocator'] == 'system'
    assert allocator['sha256'] == selected['binarySha256']
    assert {'malloc', 'calloc'}.issubset(allocator['dynamicSymbols'])
    assert all(section == 'SHN_UNDEF' for section in allocator['dynamicSymbols'].values())
    assert (directory / 'source-revision.txt').read_text().strip() == pin['revision']
    assert (directory / 'version.txt').read_text().strip() == pin['expectedVersionOutput']
    assert (directory / 'protocol.txt').read_text().strip() == inputs['privateContainerProtocol']
    assert hashlib.sha256((directory / 'catalog.json').read_bytes()).hexdigest() == pin['compatibilityCatalog']['sha256']
    smoke = json.loads((directory / 'centos82-smoke.json').read_text())
    assert smoke['passed'] is True and smoke['glibc'] == 'glibc 2.28' and smoke['httpStatus'] == 200
    assert smoke['allocator'] == 'system'
    assert smoke['response'] == 'glibc228-worker-ok'


if __name__ == '__main__':
    verify()
