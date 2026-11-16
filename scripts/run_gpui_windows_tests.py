#!/usr/bin/env python3
"""Check a production GPUI Windows window against a marked, isolated fixture.

Uses SendInput and the real HWND, never writes the application's UI model.
Requires Pillow for screenshots. Run ui_fixture.py first with Python UTF-8 mode.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import time
import traceback

from windows_ui_driver import WindowsDriver, u

ROOT = Path(__file__).resolve().parents[1]


def recycle_items(fixture):
    """Inspect only Recycle Bin entries originating under this fixture."""
    env = dict(os.environ, EVERYTHING_RECYCLE_TEST_ROOT=str(fixture / 'files'))
    command = r'''
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$root = [IO.Path]::GetFullPath($env:EVERYTHING_RECYCLE_TEST_ROOT).TrimEnd('\')
$shell = New-Object -ComObject Shell.Application
$found = @(foreach ($item in $shell.NameSpace(10).Items()) {
    $from = [string]$item.ExtendedProperty('System.Recycle.DeletedFrom')
    if ($from -eq $root -or $from.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) {
        [pscustomobject]@{ path=$item.Path; original=[IO.Path]::Combine($from, $item.Name) }
    }
})
ConvertTo-Json -InputObject $found -Compress
'''
    result = subprocess.run(['powershell', '-NoProfile', '-Command', command], env=env,
                            capture_output=True, text=True, encoding='utf-8', check=True,
                            creationflags=subprocess.CREATE_NO_WINDOW, timeout=20)
    return json.loads(result.stdout)


def restore_recycle(paths):
    if not paths:
        return
    env = dict(os.environ, EVERYTHING_RECYCLE_RESTORE=json.dumps(paths))
    command = r'''
$ErrorActionPreference = 'Stop'
$paths = [string[]](ConvertFrom-Json -InputObject $env:EVERYTHING_RECYCLE_RESTORE)
$shell = New-Object -ComObject Shell.Application
foreach ($path in $paths) {
    $item = $shell.NameSpace(10).Items() | Where-Object { [string]$_.Path -eq $path } | Select-Object -First 1
    if ($null -eq $item) { throw "Fixture Recycle Bin item not found: $path" }
    $original = [IO.Path]::Combine([string]$item.ExtendedProperty('System.Recycle.DeletedFrom'), $item.Name)
    # Restore is the first Recycle Bin verb, independent of display language.
    $item.Verbs().Item(0).DoIt()
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    while (-not (Test-Path -LiteralPath $original) -and [DateTime]::UtcNow -lt $deadline) {
        Start-Sleep -Milliseconds 50
    }
    if (-not (Test-Path -LiteralPath $original)) { throw "Fixture restore failed: $original" }
}
'''
    subprocess.run(['powershell', '-NoProfile', '-Command', command], env=env,
                   capture_output=True, check=True, timeout=20,
                   creationflags=subprocess.CREATE_NO_WINDOW)


class App:
    def __init__(self, binary, fixture, output, wait_ready=True, **extra):
        token = str(time.time_ns())
        self.state_path = output / f'state-{token}.json'
        self.log_path = output / f'app-{token}.log'
        env = dict(os.environ)
        for name in list(env):
            if name.startswith('EVERYTHING_UI_'):
                env.pop(name)
        env.update(EVERYTHING_UI_TEST='1', EVERYTHING_DATA_DIR=str(fixture / 'data'),
                   EVERYTHING_UI_STATE=str(self.state_path),
                   EVERYTHING_UI_METRICS=str(output / f'metrics-{token}.jsonl'), **extra)
        self.log = self.log_path.open('w', encoding='utf-8')
        self.process = subprocess.Popen([str(binary)], env=env, stdout=self.log,
                                        stderr=subprocess.STDOUT,
                                        creationflags=subprocess.CREATE_NO_WINDOW)
        try:
            self.driver = WindowsDriver(self.process.pid, self.state_path)
            if wait_ready:
                self.driver.wait(lambda s: not s['busy'] and s['loaded'] > 0)
        except Exception:
            self.close()
            raise

    def close(self):
        if self.process.poll() is None:
            if hasattr(self, 'driver'):
                u.PostMessageW(self.driver.hwnd, 0x0010, 0, 0)  # WM_CLOSE
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                self.process.wait(timeout=5)
        self.log.close()


def run(binary, fixture, output):
    fixture, output = fixture.resolve(), output.resolve()
    if not (fixture / '.everything-ui-fixture').is_file():
        raise RuntimeError('Refusing filesystem actions outside a marked UI fixture')
    manifest = json.loads((fixture / 'fixture.json').read_text(encoding='utf-8'))
    output.mkdir(parents=True, exist_ok=True)
    prefs = fixture / 'data/gpui-preferences.json'
    prefs.write_text(json.dumps(dict(theme='system', columns=[0, 0, 0, 0],
                                    window=[100, 100, 1000, 650])), encoding='utf-8')
    files = fixture / 'files'
    originals = {n: (files / n).read_bytes() for n in manifest['realFiles']}
    initial_recycle = {item['path'] for item in recycle_items(fixture)}
    cases = []
    app = None

    def capture(name):
        d.capture(output / f'{name}.png')

    def case(identifier, title, fn):
        started = time.monotonic()
        try:
            fn()
            cases.append(dict(id=identifier, title=title, status='pass',
                              elapsedMs=round((time.monotonic()-started)*1000, 1)))
            print(f'PASS {identifier}: {title}', flush=True)
        except Exception as error:
            cases.append(dict(id=identifier, title=title, status='fail', error=str(error),
                              traceback=traceback.format_exc()))
            print(f'FAIL {identifier}: {str(error)[:450]}', flush=True)
            try:
                capture(f'{identifier}-failure')
                d.key('escape')
                d.key('escape')
            except Exception:
                pass

    def query(text, count=None, loaded=None):
        s = d.wait_query(text)
        assert s['error'] is None, s
        if count is not None:
            assert s['total'] == count, s
        if loaded is not None:
            assert s['loaded'] == loaded, s
        return s

    def selected(text):
        query(text, 1)
        d.key('down')
        return d.wait(lambda s: len(s['selected']) == 1)

    def row_y(index=0):
        s = d.state()
        return s['resultsTop'] + s.get('measuredRowHeight', s['rowHeight']) * (index + 0.5)

    def header_y():
        s = d.state()
        return s['resultsTop'] - 12.5 * s['zoom']

    def removed_without_dialog(paths, action):
        def done(s):
            assert s['modal'] is None, 'Windows deletion must not display a confirmation'
            return (not s['actionBusy'] and s['completedAction'] == action
                    and all(not path.exists() for path in paths))
        return d.wait(done)

    try:
        app = App(binary, fixture, output)
        d = app.driver

        def startup():
            s = d.wait(lambda s: not s['busy'] and s['loaded'] == 500 and s['totalKnown'])
            assert 'renamePending' in s, 'Binary predates merge e70ecd9; rebuild it'
            assert s['total'] == manifest['entries'] and s['error'] is None, s
            capture('01-startup')
            d.text('한글')
            s = d.wait(lambda s: s['query'] == '한글' and not s['busy'])
            assert [r['name'] for r in s['rows']] == ['한글 보고서.txt'], s
            capture('02-korean')
            query('', manifest['entries'], 500)
        case('WIN-01', 'Startup frame, 500k index, focus and Korean committed input', startup)

        def grammar():
            for text, count in [('Alpha', 2), ('ALPHA', 2), ('space name', 1),
                                ('한글', 1), ('unicode-École', 1),
                                ('no-such-gpui-file-xyz', 0), ('gpui-tc-?.txt', 0)]:
                query(text, count)
            query('*.md', (manifest['virtualEntries'] + 6)//8 + 2)
            for text in ['folder-demo/', 'folder-demo\\',
                         str(files / 'folder-demo') + '\\', (files / 'folder-demo').as_posix() + '/']:
                s = query(text, 1)
                assert s['rows'][0]['name'] == 'child.txt', s
            query('report', manifest['virtualEntries'] + 1, 500)
            capture('03-results')
        case('WIN-02', 'Name/case/space/Unicode/glob and Windows absolute/relative paths', grammar)

        def stale():
            for text in ['report', 'Alpha', '*.md', '한글']:
                d.query(text)
            s = d.wait(lambda s: s['query'] == '한글' and not s['busy'])
            assert s['loaded'] == 1 and s['rows'][0]['name'] == '한글 보고서.txt', s
            query('r', loaded=100)
        case('WIN-03', 'Rapid query replacement and single-character limit', stale)

        def pagination():
            query('report', loaded=500)
            d.key('down')
            for _ in range(70):
                d.key('pagedown')
                if d.state()['loaded'] >= 1000:
                    break
            s = d.wait(lambda s: s['loaded'] >= 1000 and not s['paging'])
            assert s['primary'] >= 200, s
            capture('04-pagination')
        case('WIN-04', 'PageDown scrolling and incremental result loading', pagination)

        # Read the current layout so titlebar and zoom changes keep clicks aligned.
        def sorting():
            query('gpui-tc-', 4)
            d.click(70, header_y())
            s = d.wait(lambda s: s['sortBy'] == 'name' and s['sortDir'] == 'desc' and not s['busy'])
            assert s['rows'][0]['name'] == 'gpui-tc-trash.txt', s
            widths = s['columns']
            d.click(sum(widths[:2])+30, header_y())
            s = d.wait(lambda s: s['sortBy'] == 'size' and not s['busy'])
            assert [r['size'] for r in s['rows']] == sorted(r['size'] for r in s['rows']), s
            d.click(70, header_y())
            d.wait(lambda s: s['sortBy'] == 'name' and s['sortDir'] == 'asc' and not s['busy'])
            width = d.state()['columns'][0]
            d.send('down', x=width-2, y=header_y())
            d.send('move', x=width+78, y=header_y())
            d.send('up', x=width+78, y=header_y())
            d.wait(lambda s: s['columns'][0] >= width+70)
            capture('05-columns')
        case('WIN-05', 'Server sorting and native mouse column resize', sorting)

        def selection():
            query('gpui-tc-', 4)
            d.click(90, row_y(0))
            d.click(90, row_y(2), modifiers=['ctrl'])
            d.wait(lambda s: len(s['selected']) == 2)
            d.click(90, row_y(3), modifiers=['shift'])
            d.wait(lambda s: len(s['selected']) == 2)
            d.key('shift-up')
            d.key('ctrl-a')
            d.wait(lambda s: len(s['selected']) == 4)
            d.key('ctrl-c')
            s = d.wait(lambda s: not s['actionBusy'] and (s['toast'] or '').startswith('Copied 4 path'))
            assert d.clipboard()['text'].splitlines() == s['selected']
            d.key('escape')
            d.wait(lambda s: not s['selected'])
        case('WIN-06', 'Ctrl/Shift selection, Ctrl+A and real clipboard', selection)

        def unicode_clipboard():
            for name in ['한글 보고서.txt', 'unicode-École.md']:
                s = selected(name)
                d.key('ctrl-c')
                d.wait(lambda s: not s['actionBusy'] and (s['toast'] or '').startswith('Copied 1 path'))
                assert d.clipboard()['text'] == s['selected'][0], d.clipboard()
        case('WIN-07', 'Unicode path clipboard via Ctrl+C', unicode_clipboard)

        def context_menu():
            s = selected('한글 보고서.txt')
            d.click(100, row_y(), button='right')
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    _, _, items = d.menu()
                    break
                except RuntimeError:
                    time.sleep(0.05)
            else:
                raise AssertionError('Explorer context menu did not open')
            assert items[:3] == ['Open', 'Reveal in Explorer', 'Copy Path'], items
            d.choose_menu('Copy Path')
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline and d.clipboard()['text'] != s['selected'][0]:
                time.sleep(0.025)
            assert d.clipboard()['text'] == s['selected'][0], d.clipboard()
        case('WIN-08', 'Native Explorer context menu and Unicode Copy Path', context_menu)

        def rename():
            selected('gpui-tc-rename.txt')
            d.key('f2')
            d.wait(lambda s: s['renamePath'] is not None)
            d.text('gpui-tc-renamed')
            d.wait(lambda s: s['renameValue'] == 'gpui-tc-renamed.txt')
            d.key('enter')
            d.wait(lambda s: not s['actionBusy'] and s['renamePath'] is None)
            assert not (files/'gpui-tc-rename.txt').exists()
            assert (files/'gpui-tc-renamed.txt').read_bytes() == originals['gpui-tc-rename.txt']
            d.key('f2')
            d.key('ctrl-a')
            d.text('gpui-tc-collision.txt')
            d.key('enter')
            s = d.wait(lambda s: not s['actionBusy'] and (s['toast'] or '') != 'Renamed')
            assert s['renamePath'] is not None, s
            capture('06-rename-collision')
            d.key('ctrl-a')
            d.text('bad/name.txt')
            d.key('enter')
            d.wait(lambda s: not s['actionBusy'] and s['renamePath'] is not None)
            d.key('ctrl-a')
            d.text('gpui-tc-rename.txt')
            d.key('enter')
            d.wait(lambda s: not s['actionBusy'] and s['renamePath'] is None)
            assert (files/'gpui-tc-rename.txt').read_bytes() == originals['gpui-tc-rename.txt']
        case('WIN-09', 'F2 rename, extension selection, collision and invalid-name errors', rename)

        def trash():
            selected('gpui-tc-trash.txt')
            d.key('delete')
            s = removed_without_dialog([files/'gpui-tc-trash.txt'], 'TrashImmediately')
            assert s['loaded'] == 0, s
            capture('07-delete-without-confirmation')
            query('gpui-tc-', 3, 3)
        case('WIN-10', 'Windows Delete: Recycle Bin without confirmation and result suppression', trash)

        def minimum_window():
            s = d.state()
            d.click(s['width']-29*s['zoom'], s['height']-16*s['zoom'])
            d.wait(lambda s: s['modal'] == 'settings')
            d.key('down')
            d.key('down')
            d.key('enter')
            d.wait(lambda s: 'disabled in isolated' in (s['toast'] or ''))
            d.resize(400, 300)
            d.wait(lambda s: s['width'] == 400 and s['height'] == 300)
            selected('gpui-tc-open.txt')
            capture('08-minimum-window')
        case('WIN-11', 'Fixture Reset guard and minimum-size search results', minimum_window)

        def persistence():
            nonlocal app, d
            d.resize(1010, 610)
            d.wait(lambda s: s['width'] == 1010 and s['height'] == 610)
            before = d.state()
            time.sleep(0.5)
            app.close()
            saved = json.loads(prefs.read_text(encoding='utf-8'))
            assert saved['theme'] == 'system', saved
            app = App(binary, fixture, output)
            d = app.driver
            s = d.state()
            assert (s['width'], s['height']) == (before['width'], before['height']), s
            assert s['columns'] == before['columns'], s
            app.close()
            app = App(binary, fixture, output)
            d = app.driver
            s = d.state()
            assert s['height'] == before['height'], s
            capture('09-restart')
        case('WIN-12', 'Window/column preferences across two restarts', persistence)

        def merged_gestures():
            selected('gpui-tc-open.txt')
            time.sleep(u.GetDoubleClickTime()/1000 + 0.1)
            d.click(90, row_y())
            time.sleep(d.state()['renameDelayMs']/1000 + 0.1)
            s = d.state()
            assert s['renamePath'] is None and not s['renamePending'], s
            d.key('f2')
            d.wait(lambda s: s['renamePath'] is not None)
            d.key('escape')
            d.wait(lambda s: s['renamePath'] is None)
            for key, action in [('enter', 'Open'), ('ctrl-o', 'Open'), ('ctrl-enter', 'Reveal')]:
                d.key(key)
                d.wait(lambda s: not s['actionBusy'] and s['completedAction'] == action)
            d.activate()
            time.sleep(u.GetDoubleClickTime()/1000 + 0.1)
            d.click(90, row_y(), count=2)
            d.wait(lambda s: not s['actionBusy'] and s['completedAction'] == 'Open')
            time.sleep(d.state()['renameDelayMs']/1000 + 0.1)
            s = d.state()
            assert s['renamePath'] is None and not s['renamePending'], s
            d.key('ctrl-f')
            d.key('ctrl-a')
            d.key('ctrl-c')
            time.sleep(0.1)
            assert d.clipboard()['text'] == 'gpui-tc-open.txt', d.clipboard()
            assert not d.state()['selected'], d.state()
            capture('10-merged-windows-gestures')
        case('WIN-13', 'e70ecd9: Windows F2, selected click, Enter/Ctrl+O/Reveal, double click, input clipboard', merged_gestures)

        def batch_trash():
            query('', manifest['entries'], 500)  # Fresh model after WIN-12's restart
            names = ['child.txt', 'folder-demo', 'notes.md', 'Alpha report.txt']
            rows = d.state()['rows']
            for i, name in enumerate(names):
                index = next(i for i, row in enumerate(rows) if row['name'] == name)
                d.click(90, row_y(index), modifiers=['ctrl'] if i else [])
            s = d.wait(lambda s: len(s['selected']) == 4)
            assert set(s['selected']) == {str(files/name) for name in
                                          ['folder-demo/child.txt', 'folder-demo', 'notes.md', 'Alpha report.txt']}, s
            capture('11-batch-trash')
            d.key('delete')
            removed_without_dialog([files/name for name in ['folder-demo', 'notes.md', 'Alpha report.txt']], 'TrashImmediately')
            for name in ['folder-demo', 'notes.md', 'Alpha report.txt']:
                assert not (files/name).exists(), name
            assert (files/'folder-demo-other/keep.txt').read_bytes() == originals['folder-demo-other/keep.txt']
            recycled = {item['original']: item['path'] for item in recycle_items(fixture)
                        if item['path'] not in initial_recycle}
            assert Path(recycled[str(files/'folder-demo')]).joinpath('child.txt').read_bytes() == originals['folder-demo/child.txt']
            for name in ['notes.md', 'Alpha report.txt', 'gpui-tc-trash.txt']:
                assert Path(recycled[str(files/name)]).read_bytes() == originals[name], name
            query('folder-demo/', 0)
            query('folder-demo-other/', 1)
        case('WIN-14', 'e70ecd9: batch Trash with selected folder+child, Recycle Bin contents, sibling preservation', batch_trash)

        def permanent_delete():
            before = {item['path'] for item in recycle_items(fixture)}
            selected('unicode-École.md')
            d.key('shift-delete')
            removed_without_dialog([files/'unicode-École.md'], 'DeletePermanently')
            query('unicode-École.md', 0, 0)
            query('')
            rows = d.state()['rows']
            for i, name in enumerate(['Info.plist', 'Contents', 'Demo.app']):
                index = next(i for i, row in enumerate(rows) if row['name'] == name)
                d.click(90, row_y(index), modifiers=['ctrl'] if i else [])
            d.wait(lambda s: len(s['selected']) == 3)
            d.key('shift-delete')
            removed_without_dialog([files/'Demo.app'], 'DeletePermanently')
            assert (files/'folder-demo-other/keep.txt').read_bytes() == originals['folder-demo-other/keep.txt']
            assert {item['path'] for item in recycle_items(fixture)} == before, 'Shift+Delete must bypass the Recycle Bin'
            query('Demo.app', 0, 0)
            capture('12-permanent-delete')
            query('gpui-tc-open.txt', 1)
            d.key('ctrl-a')
            d.key('shift-delete')
            time.sleep(0.1)
            assert (files/'gpui-tc-open.txt').read_bytes() == originals['gpui-tc-open.txt']
            assert d.state()['modal'] is None
        case('WIN-15', 'Windows Shift+Delete: permanent file/folder removal without dialog or Recycle Bin; input focus guard', permanent_delete)

        def titlebar_drag():
            d.resize(1010, 610)
            d.wait(lambda s: s['width'] == 1010 and s['height'] == 610)
            for x, dx in [(120, 90), (60, -60)]:
                assert d.hit_test(x, 15) == 2, 'Title text and background must be native HTCAPTION'
                before = d.bounds()
                d.drag(x, 15, dx, 50)
                after = d.bounds()
                assert (after['x']-before['x'])*dx > 0 and after['y']-before['y'] >= 30, (before, after)
                assert (after['width'], after['height']) == (before['width'], before['height']), (before, after)
            assert d.hit_test(1010-69, 15) == 1, 'Window buttons must stay outside the drag region'
            assert d.hit_test(120, 50) == 1, 'Search input must stay outside the drag region'
            time.sleep(u.GetDoubleClickTime()/1000 + 0.1)
            d.click(505, 15, count=2)
            d.wait(lambda s: s['width'] > 1010)
            assert u.IsZoomed(d.hwnd), 'Caption double-click must maximize once'
            s = d.state()
            d.drag(s['width']/2, 15, 80, 90)
            d.wait(lambda s: s['width'] <= 1010)
            assert not u.IsZoomed(d.hwnd), 'Dragging a maximized caption must restore the window'
            d.resize(1010, 610)
            d.wait(lambda s: s['width'] == 1010 and s['height'] == 610)
            capture('14-titlebar-drag')
        case('WIN-17', 'Native titlebar background/text drag, caption double-click and drag from maximized window', titlebar_drag)

        def window_controls():
            d.key('escape')
            s = d.state()
            d.click(s['width']-69, 15)
            deadline = time.monotonic()+5
            while not u.IsZoomed(d.hwnd) and time.monotonic() < deadline:
                time.sleep(0.05)
            assert u.IsZoomed(d.hwnd), 'Maximize button did not maximize the HWND'
            s = d.wait(lambda s: s['width'] > 1010)
            d.click(s['width']-69, 15)
            d.wait(lambda s: s['width'] == 1010 and s['height'] == 610)
            capture('13-window-controls')
            d.click(1010-115, 15)
            time.sleep(0.1)
            assert u.IsIconic(d.hwnd), 'Minimize button did not minimize the HWND'
            d.activate()
            assert not u.IsIconic(d.hwnd)
            d.click(1010-23, 15)
            assert app.process.wait(timeout=5) == 0
        case('WIN-16', 'Visible minimize/maximize/restore/close window controls', window_controls)
    finally:
        if app:
            app.close()
        new_recycle = [item for item in recycle_items(fixture) if item['path'] not in initial_recycle]
        try:
            restore_recycle([item['path'] for item in new_recycle])
            assert not [item for item in recycle_items(fixture) if item['path'] not in initial_recycle]
        except Exception as error:
            cases.append(dict(id='WIN-CLEANUP', title='Restore only this run\'s fixture Recycle Bin items',
                              status='fail', error=str(error), traceback=traceback.format_exc()))
        for name, data in originals.items():
            if not (files/name).exists():
                (files/name).parent.mkdir(parents=True, exist_ok=True)
                (files/name).write_bytes(data)
        renamed = files/'gpui-tc-renamed.txt'
        if renamed.exists():
            renamed.unlink()
        assert all((files/name).read_bytes() == data for name, data in originals.items()), 'Fixture content changed'
    report = dict(binary=str(binary), sha256=hashlib.file_digest(binary.open('rb'), 'sha256').hexdigest(),
                  fixture=str(fixture), platform=platform.platform(),
                  date=time.strftime('%Y-%m-%dT%H:%M:%S%z'),
                  passed=sum(c['status'] == 'pass' for c in cases),
                  failed=sum(c['status'] == 'fail' for c in cases), cases=cases)
    (output/'ui-results.json').write_text(json.dumps(report, ensure_ascii=False, indent=2)+'\n', encoding='utf-8')
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT/'app/target/release/everything.exe')
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=ROOT/'.artifacts/windows-gpui/ui')
    args = parser.parse_args()
    report = run(args.binary.resolve(), args.fixture, args.output)
    print(json.dumps(dict(passed=report['passed'], failed=report['failed'])))
    raise SystemExit(bool(report['failed']))
