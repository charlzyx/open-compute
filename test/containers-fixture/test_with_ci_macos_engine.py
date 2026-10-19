"""Exercise profile ownership and failure cleanup without creating a VM."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('ci_macos_engine', HERE / 'with-ci-macos-engine.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class MacEngine(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='oc-ci-', dir='/tmp')
        self.parent = Path(self.temp.name).resolve()
        self.root = self.parent / 'vm'
        self.evidence = self.parent / 'evidence'
        self.calls = []
        self.failed_stage = None
        self.command_exit = 0
        self.interrupt = False
        owner = self

        def run(args, **kwargs):
            self.calls.append(args)
            env = kwargs['env']
            self.assertEqual(env['COLIMA_HOME'], str(self.root / 'colima'))
            self.assertEqual(env['DOCKER_CONFIG'], str(self.root / 'docker-config'))
            self.assertNotIn('LIMA_HOME', env)
            stage = args[3] if '--profile' in args else 'version'
            if stage == 'version':
                kwargs['stdout'].write(b'colima version 0.10.3\n')
            if stage == 'ssh':
                self.assertEqual(kwargs['input'], fixture.GUEST_SETUP.encode())
            return subprocess.CompletedProcess(args, 1 if stage == self.failed_stage else 0)

        class Child:
            pid = 12345

            def __init__(self, args, **kwargs):
                owner.calls.append(args)
                owner.assertTrue(kwargs['start_new_session'])
                owner.assertEqual(args[args.index('--command') + 1:],
                                  ['controlled-gate', '--unchanged-scope'])
                self.returncode = None
                self.wait_count = 0

            def wait(self, **kwargs):
                self.wait_count += 1
                if owner.interrupt and self.wait_count == 1:
                    raise KeyboardInterrupt
                self.returncode = owner.command_exit
                profile = owner.root / 'fixture'
                profile.mkdir(exist_ok=True)
                (profile / 'result.json').write_text(json.dumps({
                    'success': owner.command_exit == 0, 'mainExecuted': True,
                    'mainExitCode': owner.command_exit}))
                logs = profile / 'broker/evidence'
                logs.mkdir(parents=True, exist_ok=True)
                (logs / 'command.log').write_text('controlled command output\n')
                (logs / 'broker.stderr').write_text('controlled sanitized Broker output\n')
                return self.returncode

            def poll(self):
                return self.returncode

        self.patches = [patch.dict(os.environ, {'GITHUB_ACTIONS': 'true', 'LIMA_HOME': '/do-not-touch'}),
            patch.object(fixture.platform, 'system', return_value='Darwin'),
            patch.object(fixture.platform, 'machine', return_value='x86_64'),
            patch.object(fixture.os, 'getuid', return_value=501),
            patch.object(fixture.shutil, 'disk_usage', return_value=type('Disk', (), {'free': 50 << 30})()),
            patch.object(fixture.shutil, 'which', return_value='/controlled/tool'),
            patch.object(fixture.subprocess, 'run', run),
            patch.object(fixture.subprocess, 'Popen', Child),
            patch.object(Path, 'is_socket', return_value=True)]
        for value in self.patches:
            value.start()

    def tearDown(self):
        for value in reversed(self.patches):
            value.stop()
        self.temp.cleanup()

    def execute(self):
        return fixture.run(self.parent / 'images', self.parent / 'ocd',
                           self.root, self.evidence, ['controlled-gate', '--unchanged-scope'])

    def test_native_owned_profile_preserves_scope_and_deletes_its_data(self):
        result = self.execute()
        self.assertTrue(result['success'])
        self.assertTrue(result['ownedVmDeleted'])
        self.assertFalse(result['fullWorkspaceQualifiedByWrapper'])
        start = next(call for call in self.calls if 'start' in call)
        for flag, value in (('--cpus', '2'), ('--memory', '4'), ('--root-disk', '8'),
                            ('--disk', '16'), ('--mount', 'none')):
            self.assertEqual(start[start.index(flag) + 1], value)
        for flag in ('--ssh-agent=false', '--ssh-config=false', '--activate=false', '--template=false'):
            self.assertIn(flag, start)
        self.assertEqual(self.calls[-1], ['colima', '--profile', 'fixture', 'delete', '--force', '--data'])
        self.assertEqual(self.root.stat().st_mode & 0o777, 0o700)

    def test_local_invocation_cannot_create_a_vm(self):
        with patch.dict(os.environ, {'GITHUB_ACTIONS': 'false'}):
            with self.assertRaisesRegex(ValueError, '^STANDARD_INTEL_MACOS_CI_REQUIRED$'):
                self.execute()
        self.assertEqual(self.calls, [])
        self.assertFalse(self.root.exists())

    def test_low_host_budget_fails_before_creating_any_profile(self):
        with patch.object(fixture.shutil, 'disk_usage', return_value=type('Disk', (), {'free': 10 << 30})()):
            with self.assertRaisesRegex(ValueError, '^HOST_BUILD_AND_VM_RESERVE_REQUIRED$'):
                self.execute()
        self.assertEqual(self.calls, [])
        self.assertFalse(self.root.exists())

    def test_existing_root_is_never_reused_or_deleted(self):
        self.root.mkdir()
        with self.assertRaisesRegex(ValueError, '^FRESH_SHORT_OWNED_ROOTS_REQUIRED$'):
            self.execute()
        self.assertTrue(self.root.exists())
        self.assertEqual(self.calls, [])

    def test_partial_start_failure_still_deletes_only_owned_profile(self):
        self.failed_stage = 'start'
        result = self.execute()
        self.assertFalse(result['success'])
        self.assertFalse(result['mainExecuted'])
        self.assertTrue(result['ownedVmDeleted'])
        self.assertEqual(result['failureCode'], 'START_FAILED')

    def test_guest_setup_failure_does_not_execute_gate(self):
        self.failed_stage = 'ssh'
        result = self.execute()
        self.assertFalse(result['success'])
        self.assertFalse(result['mainExecuted'])
        self.assertTrue(result['ownedVmDeleted'])

    def test_failed_gate_remains_failed_after_successful_profile_cleanup(self):
        self.command_exit = 1
        result = self.execute()
        self.assertFalse(result['success'])
        self.assertTrue(result['mainExecuted'])
        self.assertEqual(result['mainExitCode'], 1)
        self.assertTrue(result['ownedVmDeleted'])
        self.assertEqual((self.evidence / 'wrapped-command.log').read_text(),
                         'controlled command output\n')
        self.assertEqual((self.evidence / 'broker.stderr').read_text(),
                         'controlled sanitized Broker output\n')

    def test_profile_cleanup_failure_cannot_report_success(self):
        self.failed_stage = 'delete'
        result = self.execute()
        self.assertFalse(result['success'])
        self.assertEqual(result['cleanupFailureClass'], 'ValueError')

    def test_interrupt_reaps_pipeline_group_before_deleting_vm(self):
        self.interrupt = True
        with patch.object(fixture.os, 'killpg') as kill:
            with self.assertRaises(KeyboardInterrupt):
                self.execute()
        kill.assert_called_once_with(12345, fixture.signal.SIGINT)
        result = json.loads((self.evidence / 'result.json').read_bytes())
        self.assertFalse(result['success'])
        self.assertTrue(result['ownedVmDeleted'])


if __name__ == '__main__':
    unittest.main()
