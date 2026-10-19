"""Schema checks; controlled contract data does not qualify a provider."""
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import socketserver
import tempfile
import threading
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('prepare_input', HERE / 'prepare-input.py')
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class Input(unittest.TestCase):
    def setUp(self):
        self.contract = {'version': 2, 'runtime': 'docker', 'engineVersion': '29.4.0',
                         'architecture': 'arm64', 'isolation': 'controlled-test-data',
                         'qualified': True, 'snapshots': True, 'sidecarImage': 'sha256:' + 'a' * 64,
                         'execHelperSha256': 'b' * 64,
                         'resources': {'cpuNanos': 62500000, 'memoryBytes': 256 << 20,
                                       'diskBytes': 2000000000, 'pids': 128, 'openFiles': 1024},
                         'maxInstances': 16, 'maxImageBytes': 1 << 30, 'maxStreamBytes': 1 << 30}

    def body(self):
        return json.dumps(self.contract, separators=(',', ':')).encode()

    def policy(self, instances=4, snapshots=32, snapshot_bytes=8 << 30):
        resources = dict(self.contract['resources'], pids=64)
        return prepare.policy(self.body(), Path('/tmp/test-broker.sock'), {
            'max_instances': instances, 'max_snapshots': snapshots,
            'max_snapshot_bytes': snapshot_bytes, 'resources': resources})

    def test_digest_uses_untouched_wire_bytes_and_resources(self):
        before = copy.deepcopy(self.contract)
        value, architecture = self.policy()
        self.assertEqual(value['provider_contract_sha256'], hashlib.sha256(self.body()).hexdigest())
        self.assertEqual(architecture, 'arm64')
        self.assertEqual(value['resources'], dict(self.contract['resources'], pids=64))
        self.assertEqual(self.contract, before)
        self.assertEqual(set(value), {'broker_socket', 'provider_contract_sha256', 'max_instances',
                                     'max_snapshots', 'max_snapshot_bytes', 'resources'})

    def test_unqualified_and_snapshot_disabled_are_rejected(self):
        for name in ('qualified', 'snapshots'):
            with self.subTest(name=name):
                self.contract[name] = False
                with self.assertRaisesRegex(ValueError, '^CONTRACT_NOT_QUALIFIED$'):
                    self.policy()
                self.contract[name] = True

    def test_unrecognized_contract_field_is_rejected(self):
        self.contract['unexpected'] = 1
        with self.assertRaisesRegex(ValueError, '^CONTRACT_FIELDS$'):
            self.policy()

    def test_noninteger_and_excess_resource_limits_are_rejected(self):
        for value in (True, 0, 1 << 63, '256'):
            with self.subTest(value=value):
                self.contract['resources']['memoryBytes'] = value
                with self.assertRaisesRegex(ValueError, '^CONTRACT_RESOURCES$'):
                    self.policy()

    def test_missing_instance_and_snapshot_limits_are_rejected(self):
        for args in ((1, 32, 8 << 30), (17, 32, 8 << 30), (4, 0, 8 << 30),
                     (4, 100001, 8 << 30), (4, 32, 0), (4, 32, 1 << 63)):
            with self.subTest(args=args):
                with self.assertRaisesRegex(ValueError, '^INPUT_LIMITS$'):
                    self.policy(*args)

    def test_wrong_or_missing_helper_identity_is_rejected(self):
        self.contract['sidecarImage'] = 'latest'
        with self.assertRaisesRegex(ValueError, '^CONTRACT_PLATFORM_OR_HELPER$'):
            self.policy()

    def test_unbounded_contract_policy_is_rejected(self):
        self.contract['maxImageBytes'] = 1 << 41
        with self.assertRaisesRegex(ValueError, '^CONTRACT_POLICY$'):
            self.policy()

    def test_requested_resource_cannot_exceed_provider_ceiling(self):
        value = {'max_instances': 4, 'max_snapshots': 32, 'max_snapshot_bytes': 8 << 30,
                 'resources': dict(self.contract['resources'], pids=129)}
        with self.assertRaisesRegex(ValueError, '^INPUT_RESOURCE_LIMITS$'):
            prepare.policy(self.body(), Path('/tmp/test-broker.sock'), value)

    def test_regular_file_cannot_replace_broker_socket(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary).resolve() / 'broker.sock'
            path.write_text('controlled data')
            with self.assertRaisesRegex(ValueError, '^BROKER_SOCKET_AUTHORITY$'):
                prepare.read_contract(path)

    def contract_server(self, path, status, body):
        lines = []

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                lines.append(self.rfile.readline(512))
                self.wfile.write(f'HTTP/1.1 {status} Controlled\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)

        server = socketserver.UnixStreamServer(str(path), Handler)
        server.timeout = 2
        os.chmod(path, 0o600)
        thread = threading.Thread(target=server.handle_request)
        thread.start()
        return server, thread, lines

    def test_contract_handshake_retains_wire_body_and_socket_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary).resolve() / 'broker.sock'
            server, thread, lines = self.contract_server(path, 200, self.body())
            try:
                body, authority = prepare.read_contract(path)
                self.assertEqual(body, self.body())
                self.assertEqual(authority, prepare.oci.identity(path.lstat()))
                self.assertEqual(lines, [b'GET /open-compute/v1/contract HTTP/1.1\r\n'])
            finally:
                thread.join(3)
                server.server_close()
                self.assertFalse(thread.is_alive())

    def test_unsuccessful_contract_handshake_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary).resolve() / 'broker.sock'
            server, thread, _ = self.contract_server(path, 503, b'controlled unavailable')
            try:
                with self.assertRaisesRegex(ValueError, '^BROKER_CONTRACT_RESPONSE$'):
                    prepare.read_contract(path)
            finally:
                thread.join(3)
                server.server_close()
                self.assertFalse(thread.is_alive())


if __name__ == '__main__':
    unittest.main()
