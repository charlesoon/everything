#!/usr/bin/env node
// Optional npm aliases. The application itself needs Rust, not Node or a WebView.
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const env = { ...process.env };
if (process.platform === 'darwin') {
  env.MACOSX_DEPLOYMENT_TARGET ||= '13.0';
  if (!env.DEVELOPER_DIR && existsSync('/Applications/Xcode.app/Contents/Developer')) {
    env.DEVELOPER_DIR = '/Applications/Xcode.app/Contents/Developer';
  }
}
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, env, stdio: 'inherit' });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status || 1);
}
const [mode = 'dev', ...args] = process.argv.slice(2);
const manifest = ['--manifest-path', 'src-tauri/Cargo.toml'];
switch (mode) {
  case 'dev': run('cargo', ['run', '--locked', ...manifest, '--', ...args]); break;
  case 'build': run('cargo', ['build', '--release', '--locked', ...manifest, ...args]); break;
  case 'check':
    run('cargo', ['check', '--locked', ...manifest, ...args]);
    break;
  case 'test': run('cargo', ['test', '--locked', ...manifest, '--no-run', ...args]); break;
  case 'test-run': run('cargo', ['test', '--locked', ...manifest, ...args]); break;
  case 'bundle':
  case 'dmg':
    if (process.platform === 'darwin') {
      run('python3', ['scripts/bundle_macos.py', ...(mode === 'dmg' ? ['--dmg'] : []), ...args]);
    } else if (process.platform === 'win32' && mode === 'bundle') {
      run('powershell.exe', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', 'scripts/build-windows.ps1', ...args]);
    } else {
      throw new Error('Packaging supports macOS (.app/.dmg) and Windows (NSIS).');
    }
    break;
  default: throw new Error(`Unknown native command: ${mode}`);
}
