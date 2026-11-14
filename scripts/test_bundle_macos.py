"""Signing, notarization and release publication checks using isolated fixtures."""

import base64
import contextlib
from datetime import datetime
import hashlib
import io
import json
from pathlib import Path
import plistlib
import subprocess
import tempfile
from types import SimpleNamespace
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


class ReleaseTests(unittest.TestCase):
    def package(self, root, payload, *, fail_notarization=False, app_only=False, target=None):
        target_directory = root / 'target'
        release = target_directory / target / 'release' if target else target_directory / 'release'
        files = {
            release / 'everything': b'Native executable fixture',
            root / 'src-tauri/icons/icon.icns': b'Icon fixture',
            root / 'src-tauri/icons/Assets.car': b'Asset catalog fixture',
            root / 'src-tauri/assets/fonts/geist-LICENSE.txt': b'Font license fixture',
            root / 'src-tauri/assets/fonts/jetbrains-mono-LICENSE.txt': b'Font license fixture',
            root / 'src-tauri/Info.plist': plistlib.dumps({}),
            root / 'src-tauri/launchd/com.everything.app.daemon.plist': plistlib.dumps({
                'BundleProgram': 'Contents/MacOS/everything',
            }),
        }
        for path, content in files.items():
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        metadata = json.dumps({
            'packages': [{'name': 'everything', 'version': '1.2.3'}],
            'target_directory': str(target_directory),
        })

        def output(args, **kwargs):
            if args[0] == 'git':
                return str(root / '.git')
            return APPLE['developer_id'] if args[0] == 'security' else metadata

        def command(*args, **kwargs):
            if args[0] == 'hdiutil':
                Path(args[-1]).write_bytes(payload)

        def notarize(dmg, env, apple):
            if fail_notarization:
                raise SystemExit('Notarization rejected in fixture')
            dmg.write_bytes(dmg.read_bytes() + b'\nStapled ticket fixture')

        arguments = ['bundle_macos.py', '--skip-build']
        if not app_only:
            arguments.append('--dmg')
        if target:
            arguments += ['--target', target]
        with contextlib.ExitStack() as stack:
            stack.enter_context(patch.object(bundle_macos, 'ROOT', root))
            stack.enter_context(patch.object(bundle_macos.sys, 'argv', arguments))
            stack.enter_context(patch.object(bundle_macos.os, 'uname', return_value=SimpleNamespace(
                sysname='Darwin', machine='arm64')))
            stack.enter_context(patch.object(bundle_macos, 'signing_settings',
                                            return_value=(APPLE['developer_id'], APPLE)))
            stack.enter_context(patch.object(bundle_macos.subprocess, 'check_output', side_effect=output))
            stack.enter_context(patch.object(bundle_macos, 'run', side_effect=command))
            stack.enter_context(patch.object(bundle_macos, 'notarize', side_effect=notarize))
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            bundle_macos.main()

    def test_rebuilding_refreshes_release_from_final_stapled_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            name = 'Everything_1.2.3_aarch64.dmg'
            for payload in [b'First DMG fixture', b'Rebuilt DMG fixture' * 70000]:
                self.package(root, payload)
                expected = payload + b'\nStapled ticket fixture'
                self.assertEqual((root / 'releases' / name).read_bytes(), expected)
                self.assertEqual((root / 'target/release/bundle/dmg' / name).read_bytes(), expected)
                manifest = (root / 'releases/latest-mac.yml').read_text()
                digest = base64.b64encode(hashlib.sha512(expected).digest()).decode('ascii')
                self.assertIn('version: 1.2.3\n', manifest)
                self.assertIn(f'url: {name}\n', manifest)
                self.assertIn(f'path: {name}\n', manifest)
                self.assertEqual(manifest.count(f'sha512: {digest}\n'), 2)
                self.assertIn(f'size: {len(expected)}\n', manifest)
                date = manifest.split("releaseDate: '", 1)[1].strip().strip("'")
                self.assertEqual(datetime.fromisoformat(date.replace('Z', '+00:00')).utcoffset().total_seconds(), 0)
                self.assertEqual(set(p.name for p in (root / 'releases').iterdir()),
                                 {name, 'latest-mac.yml'})

    def test_failed_notarization_keeps_previous_release_and_manifest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.package(root, b'Previous DMG fixture')
            releases = root / 'releases'
            before = {p.name: p.read_bytes() for p in releases.iterdir()}
            with self.assertRaises(SystemExit):
                self.package(root, b'Rejected replacement fixture', fail_notarization=True)
            self.assertEqual({p.name: p.read_bytes() for p in releases.iterdir()}, before)

    def test_app_only_build_does_not_publish_a_release(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.package(root, b'Unused DMG fixture', app_only=True)
            self.assertFalse((root / 'releases').exists())

    def test_release_filename_follows_explicit_target_architecture(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.package(root, b'Intel DMG fixture', target='x86_64-apple-darwin')
            name = 'Everything_1.2.3_x86_64.dmg'
            self.assertTrue((root / 'releases' / name).is_file())
            self.assertIn(f'path: {name}\n', (root / 'releases/latest-mac.yml').read_text())

    def test_worktree_build_also_refreshes_main_checkout_releases(self):
        with tempfile.TemporaryDirectory() as temporary:
            main = (Path(temporary) / 'main').resolve()
            worktree = (Path(temporary) / 'worktree').resolve()
            subprocess.run(['git', 'init', '-q', str(main)], check=True)
            subprocess.run([
                'git', '-C', str(main), '-c', 'user.name=Bundle Test',
                '-c', 'user.email=bundle@example.invalid', '-c', 'commit.gpgsign=false',
                'commit', '--allow-empty', '-qm', 'Fixture',
            ], check=True)
            subprocess.run(['git', '-C', str(main), 'worktree', 'add', '-qb',
                            'release-test', str(worktree)], check=True)
            (main / 'releases').mkdir()
            (main / 'releases/latest-mac.yml').write_text('version: 1.2.2\n')
            dmg = worktree / 'Everything_1.2.3_aarch64.dmg'
            dmg.write_bytes(b'Final notarized DMG fixture')
            with patch.object(bundle_macos, 'ROOT', worktree):
                image, manifest = bundle_macos.publish_dmg(dmg, '1.2.3')
            self.assertEqual(image, main / 'releases' / dmg.name)
            self.assertEqual(manifest, main / 'releases/latest-mac.yml')
            self.assertEqual(image.read_bytes(), dmg.read_bytes())
            self.assertEqual(image.read_bytes(), (worktree / 'releases' / dmg.name).read_bytes())
            self.assertEqual(manifest.read_bytes(), (worktree / 'releases/latest-mac.yml').read_bytes())
            self.assertIn('version: 1.2.3\n', manifest.read_text())


if __name__ == '__main__':
    unittest.main()
