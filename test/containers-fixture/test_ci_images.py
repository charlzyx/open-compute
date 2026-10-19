"""Check image preparation dependencies without starting CI or Docker."""
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[2]
PREPARE = 'uses: ./.github/actions/prepare-container-fixture-images'


def job(source, name):
    match = re.search(r'^  ' + re.escape(name) + r':\n(.*?)(?=^  [\w-]+:\n|\Z)',
                      source, re.MULTILINE | re.DOTALL)
    if match is None:
        raise AssertionError('Missing job: ' + name)
    return match.group(1)


class ImageWorkflowTests(unittest.TestCase):
    def test_each_build_consumer_waits_for_current_run_images(self):
        for workflow, names in [('ci.yml', ('checks', 'ai-search-qualification',
                                            's3-provider-qualification')),
                                ('release.yml', ('coverage', 'integration', 'package'))]:
            source = (ROOT / '.github/workflows' / workflow).read_text()
            for name in names:
                with self.subTest(workflow=workflow, job=name):
                    body = job(source, name)
                    self.assertRegex(body, r'needs: [^\n]*fixture-images')
                    self.assertEqual(body.count(PREPARE), 1)
                    build = body.index('bun run build' if name != 'package'
                                       else './scripts/package-release.sh')
                    self.assertLess(body.index('uses: ./.github/actions/setup-open-compute'),
                                    body.index(PREPARE))
                    self.assertLess(body.index(PREPARE), build)
            producer = job(source, 'fixture-images')
            self.assertIn('needs: failfast', producer)
            self.assertIn('uses: ./.github/workflows/container-images.yml', producer)
        ci = (ROOT / '.github/workflows/ci.yml').read_text()
        self.assertIn('arm64: false', job(ci, 'fixture-images'))
        self.assertIn("needs.failfast.outputs.scope == 'full'", job(ci, 'fixture-images'))
        self.assertIn("github.ref == 'refs/heads/main'", job(ci, 'fixture-images'))
        self.assertIn('needs.fixture-images.result', job(ci, 'ci'))

    def test_native_producer_and_consumer_use_the_same_artifact_names(self):
        source = (ROOT / '.github/workflows/container-images.yml').read_text()
        self.assertIn('workflow_call:', source)
        self.assertIn('type: boolean', source)
        self.assertIn('default: true', source)
        self.assertIn('"os":"ubuntu-24.04","architecture":"amd64"', source)
        self.assertIn('"os":"ubuntu-24.04-arm","architecture":"arm64"', source)
        self.assertIn('backend: local', source)
        self.assertIn('version: 1.22.0', source)
        self.assertNotIn('--image ', source)
        self.assertLess(source.index('./scripts/build-container-sidecar-source.sh'),
                        source.index('test/containers-fixture/build-images.py'))
        self.assertLess(source.index('test/containers-fixture/consume-images.py'),
                        source.index('uses: actions/upload-artifact@'))
        self.assertIn('name: p23-images-${{ matrix.architecture }}', source)
        self.assertIn('if-no-files-found: error', source)
        consumer = (ROOT / '.github/actions/prepare-container-fixture-images/action.yml').read_text()
        self.assertIn('X64) architecture=amd64', consumer)
        self.assertIn('ARM64) architecture=arm64', consumer)
        self.assertIn('name: p23-images-${{ steps.platform.outputs.architecture }}', consumer)
        self.assertNotIn('run-id:', consumer)
        self.assertNotIn('github-token:', consumer)
        self.assertLess(consumer.index('test/containers-fixture/consume-images.py'),
                        consumer.index('echo "OPEN_COMPUTE_TEST_CONTAINERS_INPUT='))
        self.assertNotIn('"containers":', consumer)
        self.assertNotIn('qualified-input', consumer)

    def test_build_policy_disables_automatic_gc(self):
        for name in ('.github/actions/setup-open-compute/action.yml',
                     '.github/workflows/container-images.yml',
                     'test/gateway/runtime.Dockerfile'):
            with self.subTest(file=name):
                source = (ROOT / name).read_text()
                self.assertIn('mbx settings set gc.auto false', source)
                self.assertRegex(source, r'MBX_GC_AUTO[=:] ?"?false')
                for setting in ('build_script_execution', 'target.views', 'restore_hardlink'):
                    self.assertIn('mbx settings set ' + setting + ' false', source)

    def test_release_still_requires_full_mac_final_and_coverage(self):
        source = (ROOT / '.github/workflows/release.yml').read_text()
        coverage = job(source, 'coverage')
        integration = job(source, 'integration')
        self.assertIn('./test/coverage.sh --jobs 2', coverage)
        self.assertIn("if: runner.os == 'macOS'", integration)
        self.assertEqual(integration.count('./test/gate.py --workspace --final --jobs 2'), 1)
        self.assertIn('test-p0-2-egress-linux.sh p0-2 --jobs 2', integration)
        self.assertNotIn('test-p0-2-egress-linux.sh --workspace', integration)
        self.assertIn('runs-on: macos-15-intel', coverage)
        self.assertIn('os: [ubuntu-24.04, macos-15-intel]', integration)
        for body in (coverage, integration):
            self.assertIn('uses: ./.github/actions/setup-container-fixture-engine', body)
            self.assertEqual(body.count('test/containers-fixture/with-ci-macos-engine.py'), 1)
            build = body.index('mbx build --locked -p open-compute-service --bin ocd')
            self.assertLess(build, body.index('test/containers-fixture/with-ci-macos-engine.py'))
            self.assertIn('--binary "$PWD/target/debug/ocd"', body)
            self.assertIn('.temp/containers-ci-engine/', body)
        # Package coverage remains native ARM64; only virtualization jobs move to Intel.
        self.assertIn('target: darwin-arm64\n            os: macos-15', job(source, 'package'))

    def test_intel_uses_locked_mbx_source_and_a_verified_colima_binary(self):
        source = (ROOT / '.github/actions/setup-open-compute/action.yml').read_text()
        self.assertIn('cargo install mbx --version 1.22.0 --locked', source)
        self.assertIn('test "$(mbx --version)" = \'mbx 1.22.0\'', source)
        intel = source.split('name: Restore Intel macOS Rust build cache')[1].split(
            'name: Install mbx and restore Rust build cache')[0]
        self.assertIn("runner.os == 'macOS' && runner.arch == 'X64'", intel)
        self.assertNotIn('version: 1.22.0', intel)
        engine = (ROOT / '.github/actions/setup-container-fixture-engine/action.yml').read_text()
        self.assertIn('test "$RUNNER_OS:$RUNNER_ARCH" = macOS:X64', engine)
        self.assertIn('releases/download/v0.10.3/colima-Darwin-x86_64', engine)
        self.assertIn('3082737fe8a98afda11cba7d9a20b6e56fe80c6153464beda04bec630758770b', engine)
        self.assertIn('shasum -a 256 --check', engine)
        self.assertNotIn('colima start', engine)


if __name__ == '__main__':
    unittest.main()
