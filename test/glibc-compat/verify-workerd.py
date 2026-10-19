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
    assert metadata['id'] == int(artifact_id)
    assert metadata['name'] == 'workerd-linux-x64-glibc228-experiment'
    assert not metadata['expired']
    assert metadata['workflow_run']['head_sha'] == inputs['compatibilityProducerCommit']
    assert inputs['compatibilityWorkflowRun'].endswith('/' + str(metadata['workflow_run']['id']))
    assert hashlib.sha256((ROOT / 'test/glibc-compat/Dockerfile').read_bytes()).hexdigest() == inputs['compatibilityBuilderSha256']
    directory = ROOT / '.temp/glibc-ocd/workerd'
    selected = pin['targets']['linux-x64']
    archive = (directory / selected['archiveName']).read_bytes()
    assert len(archive) <= 64 * 1024 * 1024
    assert hashlib.sha256(archive).hexdigest() == selected['archiveSha256']
    binary = gzip.decompress(archive)
    assert len(binary) <= 256 * 1024 * 1024
    assert hashlib.sha256(binary).hexdigest() == selected['binarySha256']
    assert (directory / 'source-revision.txt').read_text().strip() == pin['revision']
    assert (directory / 'version.txt').read_text().strip() == pin['expectedVersionOutput']
    assert (directory / 'protocol.txt').read_text().strip() == inputs['privateContainerProtocol']
    assert hashlib.sha256((directory / 'catalog.json').read_bytes()).hexdigest() == pin['compatibilityCatalog']['sha256']
    smoke = json.loads((directory / 'centos82-smoke.json').read_text())
    assert smoke['passed'] and smoke['glibc'] == 'glibc 2.28' and smoke['httpStatus'] == 200
    assert smoke['response'] == 'glibc228-worker-ok'
    print(json.dumps({'artifact': metadata['id'], 'producerCommit': metadata['workflow_run']['head_sha'],
                      'archiveSha256': selected['archiveSha256'], 'binarySha256': selected['binarySha256'],
                      'passed': True}))


if __name__ == '__main__':
    verify()
