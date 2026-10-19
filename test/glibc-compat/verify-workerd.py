"""Bind the reused GitHub artifact to the tracked compatibility pin and evidence."""
import hashlib
import gzip
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def verify():
    pin = json.loads((ROOT / 'packages/runtime/workerd-glibc228.lock.json').read_text())
    inputs = pin['source']['buildInputs']
    artifact_id = inputs['compatibilityArtifactId']
    metadata = json.loads(subprocess.check_output([
        'gh', 'api', f'repos/charlzyx/open-compute/actions/artifacts/{artifact_id}'], text=True))
    directory = ROOT / '.temp/glibc-ocd/workerd'
    builder_sha256 = hashlib.sha256((ROOT / 'test/glibc-compat/Dockerfile').read_bytes()).hexdigest()
    verify_evidence(pin, metadata, directory, builder_sha256)
    print(json.dumps({'artifact': metadata['id'], 'producerCommit': metadata['workflow_run']['head_sha'],
                      'archiveSha256': pin['targets']['linux-x64']['archiveSha256'],
                      'binarySha256': pin['targets']['linux-x64']['binarySha256'],
                      'allocator': 'system', 'passed': True}))


def verify_evidence(pin, metadata, directory, builder_sha256):
    inputs = pin['source']['buildInputs']
    artifact_id = inputs['compatibilityArtifactId']
    assert metadata['id'] == int(artifact_id)
    assert metadata['name'] == 'workerd-linux-x64-glibc228-system-malloc-diagnostic'
    assert not metadata['expired']
    assert metadata['workflow_run']['head_sha'] == inputs['compatibilityProducerCommit']
    assert inputs['compatibilityWorkflowRun'].endswith('/' + str(metadata['workflow_run']['id']))
    assert builder_sha256 == inputs['compatibilityBuilderSha256']
    assert (directory / 'producer-commit.txt').read_text().strip() == inputs['compatibilityProducerCommit']
    assert (directory / 'builder.sha256').read_text().split()[0] == builder_sha256
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
