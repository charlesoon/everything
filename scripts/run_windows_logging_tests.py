#!/usr/bin/env python3
"""Verify runtime Log settings and loading feedback in a real Windows window."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import time
import traceback

from run_gpui_windows_tests import App, ROOT


def run(binary, fixture, output):
    fixture, output = fixture.resolve(), output.resolve()
    if not (fixture / '.everything-ui-fixture').is_file():
        raise RuntimeError('A marked, isolated UI fixture is required')
    output.mkdir(parents=True, exist_ok=True)
    data = fixture / 'data'
    settings, prefs = data / 'setting.ini', data / 'gpui-preferences.json'
    saved = {p: p.read_bytes() if p.exists() else None for p in [settings, prefs]}
    log = data / 'logs/everything.log'
    cases, app, d = [], None, None

    def size():
        return log.stat().st_size if log.exists() else 0

    def text():
        return log.read_text(encoding='utf-8') if log.exists() else ''

    def current_log():
        marker = f'pid={app.process.pid} ==='
        contents = text()
        return contents.rpartition(marker)[2] if marker in contents else ''

    def launch(**extra):
        nonlocal app, d
        if app:
            app.close()
        app = App(binary, fixture, output, **extra)
        d = app.driver

    def wait_for(predicate, timeout=8):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if predicate():
                return
            time.sleep(.05)
        raise AssertionError('Expected log record did not arrive')

    def toggle(mouse=False):
        state = d.state()
        if state['modal'] != 'settings':
            d.click(state['width'] - 29 * state['zoom'], state['height'] - 16 * state['zoom'])
            d.wait(lambda s: s['modal'] == 'settings')
        # The final Windows settings item is the Log switch.
        before = d.state()['loggingEnabled']
        if mouse:
            state = d.state()
            d.click(state['width'] - 36 * state['zoom'], state['height'] - 60 * state['zoom'])
        else:
            for _ in range((3 - d.state()['modalSelection']) % 4):
                d.key('down')
            d.wait(lambda s: s['modalSelection'] == 3)
            d.key('enter')
        return d.wait(lambda s: s['loggingEnabled'] != before and not s['actionBusy'])

    def case(identifier, title, operation):
        started = time.monotonic()
        try:
            details = operation()
            cases.append(dict(id=identifier, title=title, status='pass',
                              elapsedMs=round((time.monotonic() - started) * 1000, 1),
                              details=details))
            print(f'PASS {identifier}: {title}', flush=True)
        except Exception as error:
            cases.append(dict(id=identifier, title=title, status='fail', error=str(error),
                              traceback=traceback.format_exc()))
            print(f'FAIL {identifier}: {str(error)[:400]}', flush=True)
            if d:
                try:
                    d.capture(output / f'{identifier}-failure.png')
                except Exception:
                    pass

    try:
        prefs.write_text(json.dumps(dict(window=[100, 100, 1100, 750], zoom=1,
                                        columns=[0, 0, 0, 0], fileTreeVisible=False)), encoding='utf-8')
        settings.unlink(missing_ok=True)

        def disabled():
            before = size()
            launch()
            assert not d.state()['loggingEnabled']
            d.wait_query('Alpha')
            time.sleep(.6)
            assert size() == before, 'Default-off launches must not append diagnostics'
        case('LOG-01', 'Missing setting.ini defaults to Log off without writing diagnostic records', disabled)

        def enabled():
            app.close()
            settings.write_text('logging=true\nretained=value\n', encoding='utf-8')
            launch()
            assert d.state()['loggingEnabled']
            wait_for(lambda: 'phase=gui.first_results event=end' in current_log())
            for marker in ['phase=search.execute event=start', 'phase=search.execute event=end',
                           'phase=search.count event=end', 'phase=search.complete event=end']:
                assert marker in current_log(), marker
            assert re.search(r'^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d{3} [+-]\d{4} \[gui\] phase=', text(), re.MULTILINE)
            d.capture(output / '01-log-on-startup.png')
        case('LOG-02', 'logging=true records timestamped search/count and first-results durations', enabled)

        def menu():
            state = toggle()
            assert state['modal'] == 'settings' and not state['loggingEnabled']
            assert 'logging=false' in settings.read_text(encoding='utf-8')
            assert 'retained=value' in settings.read_text(encoding='utf-8')
            d.capture(output / '02-log-off-menu.png')
            before = size()
            d.key('escape')
            d.wait_query('Alpha')
            time.sleep(.7)
            assert size() == before, 'Log off must stop subsequent searches from writing logs'
            state = toggle(mouse=True)
            assert state['modal'] == 'settings' and state['loggingEnabled']
            assert 'logging=true' in settings.read_text(encoding='utf-8')
            assert 'retained=value' in settings.read_text(encoding='utf-8')
            d.capture(output / '03-log-on-menu.png')
            d.key('escape')
            d.wait_query('한글')
            wait_for(lambda: size() > before)
        case('LOG-03', 'Settings Log off/on saves the shared flag, preserves other keys and controls real writes', menu)

        def external():
            settings.write_text('logging=false\nretained=value\n', encoding='utf-8')
            d.wait(lambda s: not s['loggingEnabled'])
            before = size()
            d.wait_query('Alpha')
            time.sleep(.7)
            assert size() == before
            settings.write_text('logging=true\nretained=value\n', encoding='utf-8')
            d.wait(lambda s: s['loggingEnabled'])
            d.wait_query('한글')
            wait_for(lambda: size() > before)
        case('LOG-04', 'Manual setting.ini changes apply to the running process and update the menu state', external)

        def restart():
            before = size()
            launch()
            assert d.state()['loggingEnabled']
            wait_for(lambda: size() > before and 'phase=gui.first_results event=end' in current_log())
        case('LOG-05', 'Log on persists through an actual application restart', restart)

        def loading():
            # This bounded fixture-only delay tests feedback, not performance.
            launch(wait_ready=False, EVERYTHING_UI_TEST_SEARCH_DELAY_MS='5000')
            d.wait(lambda s: s['busy'] and s['loaded'] == 0 and (s['emptyState'] or '').startswith('Loading files'))
            d.capture(output / '04-first-results-loading.png')
            d.wait(lambda s: s['busy'] and (s['emptyState'] or '').endswith('1s'))
            d.capture(output / '05-first-results-elapsed.png')
            d.resize(400,300)
            for _ in range(5):
                d.key('ctrl-plus')
            d.key('escape')
            d.wait(lambda s: s['zoom']==2 and s['busy'])
            d.capture(output / '06-small-loading-200.png')
            d.wait(lambda s: not s['busy'] and s['loaded'] > 0, timeout=20)
            wait_for(lambda: 'phase=gui.first_results event=end' in current_log())
            assert d.state()['emptyState'] is None and d.state()['searchActivity'] is None
            d.wait_query('no-such-windows-startup-test-file')
            d.wait(lambda s: s['emptyState'] == 'No matching files')
            records = current_log()
            elapsed = [int(v) for v in re.findall(r'phase=gui.first_results event=end elapsed_ms=(\d+)', records)]
            assert elapsed and elapsed[0] >= 5000
            return dict(injectedFixtureDelayMs=5000, firstResultsMs=elapsed[0])
        case('LOG-06', 'A pending initial search shows loading/elapsed feedback, then files or a distinct empty result', loading)
    finally:
        if app:
            app.close()
        if log.exists():
            (output / 'everything.log').write_bytes(log.read_bytes())
        for path, contents in saved.items():
            if contents is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(contents)

    report = dict(binary=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  passed=sum(c['status'] == 'pass' for c in cases),
                  failed=sum(c['status'] == 'fail' for c in cases), cases=cases)
    (output / 'logging-results.json').write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
    print(json.dumps(dict(passed=report['passed'], failed=report['failed'])), flush=True)
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'src-tauri/target/release/everything.exe')
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=ROOT / '.artifacts/windows-logging-startup/ui')
    args = parser.parse_args()
    result = run(args.binary.resolve(), args.fixture, args.output)
    raise SystemExit(bool(result['failed']))
