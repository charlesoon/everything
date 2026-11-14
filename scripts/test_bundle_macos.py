"""Signing configuration and notarization checks using isolated credentials."""

import contextlib
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import bundle_macos


APPLE = {
    'developer_id': 'Developer ID Application: Bundle Test (TESTTEAM)',
    'apple_id': 'bundle@example.invalid',
    'apple_password': 'test-only-app-password',
    'team_id': 'TESTTEAM',
}


class SigningTests(unittest.TestCase):
    def write_config(self, root):
        config = root / 'certificates/import.json'
        config.parent.mkdir(parents=True)
        config.write_text(json.dumps({'apple': APPLE}))

    def test_main_checkout_settings_are_found_from_a_real_worktree(self):
        with tempfile.TemporaryDirectory() as temporary:
            main = Path(temporary) / 'main'
            worktree = Path(temporary) / 'worktree'
            subprocess.run(['git', 'init', '-q', str(main)], check=True)
            subprocess.run([
                'git', '-C', str(main), '-c', 'user.name=Bundle Test',
                '-c', 'user.email=bundle@example.invalid', '-c', 'commit.gpgsign=false',
                'commit', '--allow-empty', '-qm', 'Fixture',
            ], check=True)
            subprocess.run(['git', '-C', str(main), 'worktree', 'add', '-qb',
                            'bundle-test', str(worktree)], check=True)
            self.write_config(main)
            output = io.StringIO()
            with patch.object(bundle_macos, 'ROOT', worktree), contextlib.redirect_stdout(output):
                identity, apple = bundle_macos.signing_settings({})
            self.assertEqual(identity, APPLE['developer_id'])
            self.assertEqual(apple, APPLE)
            self.assertNotIn(APPLE['apple_password'], output.getvalue())

    def test_explicit_identity_overrides_local_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.write_config(root)
            with patch.object(bundle_macos, 'ROOT', root), contextlib.redirect_stdout(io.StringIO()):
                identity, _ = bundle_macos.signing_settings({'APPLE_SIGNING_IDENTITY': 'Override'})
            self.assertEqual(identity, 'Override')

    def test_ad_hoc_build_does_not_read_local_credentials(self):
        with patch.object(bundle_macos.subprocess, 'check_output') as discovery:
            self.assertEqual(bundle_macos.signing_settings({'APPLE_SIGNING_IDENTITY': '-'}), ('-', {}))
        discovery.assert_not_called()

    def notary_result(self, status='Accepted', returncode=0, stderr=''):
        return subprocess.CompletedProcess([], returncode,
                                           json.dumps({'status': status, 'id': 'test-submission'}), stderr)

    def test_profile_overrides_local_notary_credentials(self):
        with patch.object(bundle_macos.subprocess, 'run', return_value=self.notary_result()) as submit, \
                patch.object(bundle_macos, 'run') as staple, contextlib.redirect_stdout(io.StringIO()):
            bundle_macos.notarize(Path('test.dmg'), {'NOTARY_PROFILE': 'test-profile'}, APPLE)
        args = submit.call_args.args[0]
        self.assertIn('--keychain-profile', args)
        self.assertNotIn('--password', args)
        self.assertEqual(staple.call_count, 2)

    def test_rejected_notarization_cannot_be_stapled(self):
        with patch.object(bundle_macos.subprocess, 'run', return_value=self.notary_result('Invalid')), \
                patch.object(bundle_macos, 'run') as staple, contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaises(SystemExit):
                bundle_macos.notarize(Path('test.dmg'), {}, APPLE)
        staple.assert_not_called()

    def test_failed_submission_does_not_log_credentials(self):
        result = self.notary_result(returncode=1,
                                   stderr=f"Error: {APPLE['apple_id']} {APPLE['apple_password']}")
        output = io.StringIO()
        with patch.object(bundle_macos.subprocess, 'run', return_value=result), \
                patch.object(bundle_macos, 'run') as staple, contextlib.redirect_stdout(io.StringIO()), \
                contextlib.redirect_stderr(output):
            with self.assertRaises(SystemExit) as error:
                bundle_macos.notarize(Path('test.dmg'), {}, APPLE)
        for key in ['apple_id', 'apple_password']:
            self.assertNotIn(APPLE[key], output.getvalue() + str(error.exception))
        staple.assert_not_called()


if __name__ == '__main__':
    unittest.main()
