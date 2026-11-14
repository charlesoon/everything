#!/usr/bin/env python3
"""Build/sign a native .app and optionally a DMG, without changing the source tree.

APPLE_SIGNING_IDENTITY defaults to ad-hoc (-). NOTARY_PROFILE, when set, names
an existing notarytool keychain profile and opts into notarization of the DMG.
"""
import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


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
    cargo = ['cargo', 'build', '--manifest-path', ROOT / 'src-tauri/Cargo.toml', '--release', '--locked']
    if args.target:
        cargo += ['--target', args.target]
    if not args.skip_build:
        run(*cargo, cwd=ROOT, env=env)
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version', '1', '--manifest-path', str(ROOT / 'src-tauri/Cargo.toml')],
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
    shutil.copy2(ROOT / 'src-tauri/icons/icon.icns', contents / 'Resources/icon.icns')
    licenses = contents / 'Resources/licenses'
    licenses.mkdir()
    for name in ['geist-LICENSE.txt', 'jetbrains-mono-LICENSE.txt']:
        shutil.copy2(ROOT / 'src-tauri/assets/fonts' / name, licenses / name)
    agents = contents / 'Library/LaunchAgents'
    agents.mkdir(parents=True)
    agent_name = 'com.everything.app.daemon.plist'
    shutil.copy2(ROOT / 'src-tauri/launchd' / agent_name, agents / agent_name)
    info = plistlib.loads((ROOT / 'src-tauri/Info.plist').read_bytes())
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
    identity = env.get('APPLE_SIGNING_IDENTITY', '-')
    sign = ['codesign', '--force', '--sign', identity, '--options', 'runtime', '--entitlements', ROOT / 'src-tauri/Entitlements.plist']
    if identity != '-':
        sign += ['--timestamp']
    run(*sign, app)
    run('codesign', '--verify', '--deep', '--strict', app)
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
    if env.get('NOTARY_PROFILE'):
        run('xcrun', 'notarytool', 'submit', dmg, '--keychain-profile', env['NOTARY_PROFILE'], '--wait')
        run('xcrun', 'stapler', 'staple', dmg)
        run('xcrun', 'stapler', 'validate', dmg)
    print(dmg, flush=True)


if __name__ == '__main__':
    main()
