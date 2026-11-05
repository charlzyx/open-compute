"""Keep the packaged smoke on the supported asset upload protocol."""
import base64
from email import policy
from email.parser import BytesParser
import importlib.util
from pathlib import Path
import unittest
from unittest import mock

SPEC = importlib.util.spec_from_file_location('ocd_smoke', Path(__file__).with_name('ocd-smoke.py'))
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)


class StaticAssetUploadTests(unittest.TestCase):
    def test_missing_asset_uses_session_token_and_base64_multipart(self):
        asset_hash = '4c73266e449fea54bba5a6dea074dbbd'
        prefix = '/client/v4/accounts/instance'
        request = mock.Mock(side_effect=[
            {'jwt': 'upload-token', 'buckets': [[asset_hash]]},
            {'jwt': 'completion-token'},
        ])
        self.assertEqual(SMOKE.upload_static_asset(request, prefix, 'site', 'boundary'),
                         'completion-token')
        self.assertEqual(request.call_count, 2)
        self.assertEqual(request.call_args_list[0], mock.call(
            prefix + '/workers/scripts/site/assets-upload-session', 'POST',
            {'manifest': {'/index.html': {'hash': asset_hash, 'size': 4}}}))
        args, kwargs = request.call_args_list[1]
        self.assertEqual(args[:2], (prefix + '/workers/assets/upload?base64=true', 'POST'))
        self.assertEqual(args[3], 'multipart/form-data; boundary=boundary')
        self.assertEqual(kwargs, {'credential': 'upload-token', 'status': 201})
        message = BytesParser(policy=policy.default).parsebytes(
            ('Content-Type: ' + args[3] + '\r\n\r\n').encode() + args[2])
        parts = list(message.iter_parts())
        self.assertEqual(len(parts), 1)
        self.assertEqual(parts[0].get_param('name', header='content-disposition'), asset_hash)
        self.assertEqual(parts[0].get_content_type(), 'text/html')
        self.assertEqual(base64.b64decode(parts[0].get_payload(), validate=True), b'good')

    def test_existing_asset_reuses_completion_without_upload(self):
        request = mock.Mock(return_value={'jwt': 'completion-token', 'buckets': []})
        self.assertEqual(SMOKE.upload_static_asset(request, '/account', 'site', 'boundary'),
                         'completion-token')
        self.assertEqual(request.call_count, 1)

    def test_unexpected_bucket_fails_before_upload(self):
        request = mock.Mock(return_value={'jwt': 'upload-token', 'buckets': [['other-hash']]})
        with self.assertRaisesRegex(AssertionError, 'Unexpected static asset upload buckets'):
            SMOKE.upload_static_asset(request, '/account', 'site', 'boundary')
        self.assertEqual(request.call_count, 1)


if __name__ == '__main__':
    unittest.main()
