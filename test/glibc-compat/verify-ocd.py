"""Verify the exact packaged executable and permit only validation-only changes."""
import hashlib
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def main():
    pin = json.loads((ROOT / 'test/glibc-compat/ocd-artifact.json').read_text())
    metadata = json.loads(subprocess.check_output([
        'gh', 'api', f'repos/{pin["repository"]}/actions/artifacts/{pin["artifactId"]}'], text=True))
    assert metadata['id'] == pin['artifactId'] and not metadata['expired']
    assert metadata['workflow_run']['id'] == pin['runId']
    assert metadata['workflow_run']['head_sha'] == pin['producerCommit']
    assert metadata['name'] == 'ocd-linux-x64-glibc228-experiment'
    changes = subprocess.check_output(['git', 'diff', '--name-only', pin['producerCommit'], 'HEAD'], cwd=ROOT, text=True).splitlines()
    assert all(name.startswith(('test/', '.github/')) for name in changes), 'Production inputs changed; rebuild instead of reusing this binary'
    source = ROOT / '.temp/glibc-ocd/source-evidence'
    report = json.loads((source / 'package.json').read_text())
    assert report['revision'] == pin['producerCommit'] and report['target'] == 'linux-x64'
    assert report['sha256'] == pin['binarySha256']
    assert report['workerdLockSha256'] == pin['workerdLockSha256']
    assert hashlib.sha256((ROOT / 'packages/runtime/workerd-glibc228.lock.json').read_bytes()).hexdigest() == pin['workerdLockSha256']
    binary = source / 'ocd-linux-x64-glibc228'
    assert binary.stat().st_size == report['bytes']
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == pin['binarySha256']
    evidence = ROOT / '.temp/glibc-ocd/evidence'
    evidence.mkdir(parents=True, exist_ok=True)
    # Artifact extraction loses executable bits; restore them only after verification.
    output = evidence / binary.name
    with output.open('xb') as stream:
        stream.write(binary.read_bytes())
    output.chmod(0o555)
    (evidence / 'package.json').write_text(json.dumps(report, indent=2) + '\n')
    result = {'producerCommit': pin['producerCommit'],
              'validationCommit': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
              'artifactId': pin['artifactId'], 'binarySha256': pin['binarySha256'], 'passed': True}
    (evidence / 'artifact-verification.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
