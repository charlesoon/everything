#!/usr/bin/env python3
"""Build/sign a native .app and publish DMGs with update metadata in releases/.

Signing and notarization use the ignored certificates/import.json in this or
the main checkout. APPLE_SIGNING_IDENTITY and NOTARY_PROFILE override those
settings; APPLE_SIGNING_IDENTITY=- explicitly selects an ad-hoc build.
"""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def main_checkout_root():
    try:
        common = Path(subprocess.check_output(
            ['git', 'rev-parse', '--path-format=absolute', '--git-common-dir'],
            cwd=ROOT, stderr=subprocess.DEVNULL, text=True).strip())
        if common.name == '.git':
            return common.parent
    except (OSError, subprocess.CalledProcessError):
        pass
    return ROOT


def signing_settings(env):
    if env.get('APPLE_SIGNING_IDENTITY') == '-':
        return '-', {}
    config = ROOT / 'certificates/import.json'
    if not config.is_file():
        config = main_checkout_root() / 'certificates/import.json'
    apple = {}
    if config.is_file():
        try:
            apple = json.loads(config.read_text())['apple']
        except (OSError, ValueError, KeyError, TypeError):
            raise SystemExit(f'Cannot read Apple signing settings from {config}') from None
        fields = ['developer_id', 'apple_id', 'apple_password', 'team_id']
        if not isinstance(apple, dict) or any(not isinstance(apple.get(key, ''), str) for key in fields):
            raise SystemExit(f'Apple signing settings must contain string values: {config}')
        print(f'Using signing settings: {config}', flush=True)
    identity = env.get('APPLE_SIGNING_IDENTITY') or apple.get('developer_id') or '-'
    return identity, apple


def notarize(dmg, env, apple):
    args = ['xcrun', 'notarytool', 'submit', str(dmg), '--wait', '--output-format', 'json']
    if env.get('NOTARY_PROFILE'):
        args += ['--keychain-profile', env['NOTARY_PROFILE']]
    else:
        args += ['--apple-id', apple['apple_id'], '--password', apple['apple_password'],
                 '--team-id', apple['team_id']]
    print('Submitting DMG for notarization...', flush=True)
    # Do not use check=True: CalledProcessError would print credential arguments.
    result = subprocess.run(args, env=env, capture_output=True, text=True)
    if result.returncode:
        output = result.stdout + result.stderr
        for key in ['apple_id', 'apple_password']:
            if apple.get(key):
                output = output.replace(apple[key], '<redacted>')
        print(output, file=sys.stderr)
        raise SystemExit('Notarization submission failed.')
    try:
        report = json.loads(result.stdout)
        status = report['status']
        submission = report['id']
    except (ValueError, KeyError, TypeError):
        raise SystemExit('Notary service returned an invalid result.') from None
    print(f'Notarization: {status} ({submission})', flush=True)
    if status != 'Accepted':
        raise SystemExit('Notarization was not accepted; no ticket was stapled.')
    run('xcrun', 'stapler', 'staple', dmg, env=env)
    run('xcrun', 'stapler', 'validate', dmg, env=env)


def publish_dmg(dmg, version):
    directories = list(dict.fromkeys([ROOT / 'releases', main_checkout_root() / 'releases']))
    release_date = datetime.now(timezone.utc).isoformat(timespec='milliseconds').replace('+00:00', 'Z')
    for releases in directories:
        releases.mkdir(parents=True, exist_ok=True)
        destination = releases / dmg.name
        manifest = releases / 'latest-mac.yml'
        with tempfile.TemporaryDirectory(prefix='.mac-release-', dir=releases) as temporary:
            staged_dmg = Path(temporary) / dmg.name
            shutil.copy2(dmg, staged_dmg)
            digest = hashlib.sha512()
            with staged_dmg.open('rb') as source:
                for chunk in iter(lambda: source.read(1024 * 1024), b''):
                    digest.update(chunk)
            sha512 = base64.b64encode(digest.digest()).decode('ascii')
            staged_manifest = Path(temporary) / manifest.name
            staged_manifest.write_text(
                f'version: {version}\n'
                f'files:\n'
                f'  - url: {dmg.name}\n'
                f'    sha512: {sha512}\n'
                f'    size: {staged_dmg.stat().st_size}\n'
                f'path: {dmg.name}\n'
                f'sha512: {sha512}\n'
                f"releaseDate: '{release_date}'\n",
                encoding='utf-8')
            staged_dmg.replace(destination)
            staged_manifest.replace(manifest)
    return destination, manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--dmg', action='store_true')
    parser.add_argument('--skip-build', action='store_true', help='package an already built release binary')
    parser.add_argument('--target', help='Rust target triple, for example aarch64-apple-darwin')
    args = parser.parse_args()
    if os.uname().sysname != 'Darwin':
        parser.error('macOS packaging must run on macOS')
    env = dict(os.environ)
    env.setdefault('MACOSX_DEPLOYMENT_TARGET', '13.0')
    if Path('/Applications/Xcode.app/Contents/Developer').is_dir():
        env.setdefault('DEVELOPER_DIR', '/Applications/Xcode.app/Contents/Developer')
    identity, apple = signing_settings(env)
    if identity != '-':
        identities = subprocess.check_output(
            ['security', 'find-identity', '-v', '-p', 'codesigning'], text=True, env=env)
        if identity not in identities:
            raise SystemExit(f'Signing identity is not installed in the keychain: {identity}')
    print(f'Signing identity: {identity}', flush=True)
    cargo = ['cargo', 'build', '--manifest-path', ROOT / 'app/Cargo.toml', '--release', '--locked']
    if args.target:
        cargo += ['--target', args.target]
    if not args.skip_build:
        run(*cargo, cwd=ROOT, env=env)
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version', '1', '--manifest-path', str(ROOT / 'app/Cargo.toml')],
        cwd=ROOT, env=env, text=True))
    package = next(p for p in metadata['packages'] if p['name'] == 'everything')
    version = package['version']
    target = Path(metadata['target_directory'])
    release = target / args.target / 'release' if args.target else target / 'release'
    binary = release / 'everything'
    if not binary.is_file():
        raise SystemExit(f'Release executable missing: {binary}')
    app = release / 'bundle/macos/Everything.app'
    if app.exists():
        shutil.rmtree(app)
    contents = app / 'Contents'
    (contents / 'MacOS').mkdir(parents=True)
    (contents / 'Resources').mkdir()
    shutil.copy2(binary, contents / 'MacOS/everything')
    shutil.copy2(ROOT / 'app/icons/icon.icns', contents / 'Resources/icon.icns')
    # Liquid Glass icon (macOS 26+). Regenerate both from everything.icon with:
    #   DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcrun actool everything.icon --compile <out> \
    #     --app-icon everything --platform macosx --minimum-deployment-target 12.0 --output-partial-info-plist <out>/p.plist
    #   then copy <out>/Assets.car and <out>/everything.icns (as icon.icns) into app/icons/.
    shutil.copy2(ROOT / 'app/icons/Assets.car', contents / 'Resources/Assets.car')
    licenses = contents / 'Resources/licenses'
    licenses.mkdir()
    for name in ['geist-LICENSE.txt', 'jetbrains-mono-LICENSE.txt']:
        shutil.copy2(ROOT / 'app/assets/fonts' / name, licenses / name)
    agents = contents / 'Library/LaunchAgents'
    agents.mkdir(parents=True)
    agent_name = 'com.everything.app.daemon.plist'
    shutil.copy2(ROOT / 'app/launchd' / agent_name, agents / agent_name)
    info = plistlib.loads((ROOT / 'app/Info.plist').read_bytes())
    info.update({
        'CFBundleExecutable': 'everything',
        'CFBundleIdentifier': 'com.everything.app',
        'CFBundleName': 'Everything',
        'CFBundleDisplayName': 'Everything',
        'CFBundlePackageType': 'APPL',
        'CFBundleShortVersionString': version,
        'CFBundleVersion': version.split('-')[0].split('+')[0],
        'LSMinimumSystemVersion': '13.0',
        'NSPrincipalClass': 'NSApplication',
        'NSHighResolutionCapable': True,
        'NSSupportsAutomaticGraphicsSwitching': True,
        'LSApplicationCategoryType': 'public.app-category.developer-tools',
    })
    (contents / 'Info.plist').write_bytes(plistlib.dumps(info))
    (contents / 'PkgInfo').write_bytes(b'APPL????')
    agent = plistlib.loads((agents / agent_name).read_bytes())
    if not (app / agent['BundleProgram']).is_file():
        raise SystemExit('LaunchAgent BundleProgram does not resolve inside the app')
    sign = ['codesign', '--force', '--sign', identity, '--options', 'runtime', '--entitlements', ROOT / 'app/Entitlements.plist']
    if identity != '-':
        sign += ['--timestamp']
    run(*sign, app, env=env)
    run('codesign', '--verify', '--deep', '--strict', app, env=env)
    print(app, flush=True)
    if not args.dmg:
        return
    arch = args.target.split('-')[0] if args.target else os.uname().machine
    arch = {'arm64': 'aarch64'}.get(arch, arch)
    dmg = release / f'bundle/dmg/Everything_{version}_{arch}.dmg'
    dmg.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='everything-dmg-') as temporary:
        stage = Path(temporary)
        run('ditto', app, stage / 'Everything.app')
        (stage / 'Applications').symlink_to('/Applications')
        run('hdiutil', 'create', '-volname', 'Everything', '-srcfolder', stage, '-ov', '-format', 'UDZO', dmg)
    credentials = all(apple.get(key) for key in ['apple_id', 'apple_password', 'team_id'])
    if env.get('NOTARY_PROFILE') or (identity != '-' and credentials):
        notarize(dmg, env, apple)
    release_dmg, manifest = publish_dmg(dmg, version)
    print(f'Updated {manifest}', flush=True)
    print(release_dmg, flush=True)


if __name__ == '__main__':
    main()
