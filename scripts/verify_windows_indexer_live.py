#!/usr/bin/env python3
"""Run the installed Windows reader against the real service and inspect logs.

Uses isolated GUI preferences. Never resets/registers the service or writes the
index. --toggle-log explicitly changes the shared Log setting, ending with On.
"""
import argparse
import ctypes as c
from ctypes import wintypes as w
import hashlib
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import time

from windows_ui_driver import WindowsDriver, u

k = c.WinDLL('kernel32', use_last_error=True)
k.CreateFileW.argtypes = [w.LPCWSTR, w.DWORD, w.DWORD, c.c_void_p, w.DWORD, w.DWORD, w.HANDLE]
k.CreateFileW.restype = w.HANDLE
k.WriteFile.argtypes = [w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p]
k.ReadFile.argtypes = [w.HANDLE, c.c_void_p, w.DWORD, c.POINTER(w.DWORD), c.c_void_p]
k.CloseHandle.argtypes = [w.HANDLE]


def service_status_once():
    # Same documented impersonation QOS as the application pipe client.
    handle = k.CreateFileW(r'\\.\pipe\EverythingIndexer', 0xC0000000, 0, None, 3, 0x120000, None)
    if handle == c.c_void_p(-1).value:
        raise c.WinError(c.get_last_error())
    try:
        request = b'{"cmd":"status"}\n'
        count = w.DWORD()
        if not k.WriteFile(handle, request, len(request), c.byref(count), None):
            raise c.WinError(c.get_last_error())
        response = c.create_string_buffer(65536)
        if not k.ReadFile(handle, response, len(response), c.byref(count), None):
            raise c.WinError(c.get_last_error())
        status = json.loads(response.raw[:count.value])
        if not status.get('ok'):
            raise RuntimeError(status)
        return status
    finally:
        k.CloseHandle(handle)


def service_status():
    # The server allows one command per interactive session. A simultaneous
    # GUI poll can briefly own that slot; retry the read-only request.
    for attempt in range(30):
        try:
            return service_status_once()
        except (OSError, RuntimeError):
            if attempt == 29:
                raise
            time.sleep(.05)


def run(binary, output, toggle_log, require_indexing, observe_seconds):
    binary, output = binary.resolve(), output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    data = output / 'gui-data'
    data.mkdir(exist_ok=True)
    (data / 'gpui-preferences.json').write_text(json.dumps(dict(window=[100, 100, 1100, 750],
         columns=[0, 0, 0, 0], zoom=1, fileTreeVisible=False)), encoding='utf-8')
    state_path = output / 'state.json'
    gui_log = data / 'logs/everything.log'
    service_data = Path(os.environ['ProgramData']) / 'com.everything.app/data'
    service_log = service_data / 'logs/service.log'
    settings = service_data.parent / 'config/setting.ini'
    env = {key: value for key, value in os.environ.items() if not key.startswith('EVERYTHING_UI_')}
    env.update(EVERYTHING_DATA_DIR=str(data), EVERYTHING_UI_STATE=str(state_path),
               EVERYTHING_UI_METRICS=str(output / 'metrics.jsonl'))
    process, driver, stream = None, None, None
    report = dict(binary=str(binary), sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                  settings=str(settings), serviceData=str(service_data), snapshots=[])

    def log_size(path):
        return path.stat().st_size if path.exists() else 0

    def launch():
        nonlocal process, driver, stream
        close()
        driver = None
        state_path.unlink(missing_ok=True)
        stream = (output / f'process-{time.time_ns()}.log').open('w', encoding='utf-8')
        process = subprocess.Popen([str(binary)], env=env, stdout=stream, stderr=subprocess.STDOUT,
                                   creationflags=subprocess.CREATE_NO_WINDOW)
        driver = WindowsDriver(process.pid, state_path)
        driver.wait(lambda s: not s['busy'] and s['loaded'] > 0, timeout=120)

    def close():
        if process and process.poll() is None:
            if driver:
                u.PostMessageW(driver.hwnd, 0x0010, 0, 0)
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
        if stream:
            stream.close()

    def wait_for(predicate, timeout=12):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if predicate():
                return
            time.sleep(.1)
        raise AssertionError('Expected live diagnostic evidence did not arrive')

    def toggle(expected):
        state = driver.state()
        if state['loggingEnabled'] == expected:
            return
        driver.click(state['width'] - 29, state['height'] - 16)
        driver.wait(lambda s: s['modal'] == 'settings')
        for _ in range(3):
            driver.key('down')
        driver.key('enter')
        driver.wait(lambda s: s['loggingEnabled'] == expected and not s['actionBusy'])
        driver.capture(output / ('log-on.png' if expected else 'log-off.png'))
        driver.key('escape')

    def snapshot():
        deadline = time.monotonic() + 5
        while True:
            service = service_status()
            state = driver.state()
            if state['indexStatus']['state'] == service['state']:
                break
            if time.monotonic() >= deadline:
                # Hidden windows intentionally defer index events. Activating
                # our own HWND flushes them before the final state comparison.
                driver.activate()
                state = driver.wait(lambda s: s['indexStatus']['state'] == service['state'], timeout=5)
                break
            time.sleep(.1)
        evidence = dict(time=time.strftime('%Y-%m-%dT%H:%M:%S%z'), service=service,
                        uiStatus=state['indexStatus'], statusText=state['statusText'],
                        loaded=state['loaded'], total=state['total'], busy=state['busy'])
        report['snapshots'].append(evidence)
        phase = state['indexStatus'].get('message')
        if require_indexing and state['indexStatus']['state'] == 'Indexing' and phase:
            assert state['statusText'].startswith(phase.split(' · ')[0]), state['statusText']
            assert 'scanned' not in state['statusText'] and 'Indexing' not in state['statusText']
            assert state['inlineIndexMessage'] is None, 'The phase must appear once in the status bar'
        return service, state

    try:
        report['serviceBeforeLaunch'] = service_status()
        launch()
        if toggle_log:
            toggle(True)
            # The next real launch records the complete initial-search timing.
            launch()
        service, state = snapshot()
        if require_indexing:
            assert service['state'] == 'Indexing', service
            assert service.get('message'), 'The live indexer must report its current phase'
        driver.capture(output / 'index-status.png')

        if state['loggingEnabled']:
            def current_log():
                contents = gui_log.read_text(encoding='utf-8') if gui_log.exists() else ''
                marker = f'pid={process.pid} ==='
                return contents.rpartition(marker)[2] if marker in contents else ''
            wait_for(lambda: 'phase=gui.first_results event=end' in current_log())
            first = re.search(r'phase=gui.first_results event=end elapsed_ms=(\d+) rows=(\d+)', current_log())
            report['firstResultsMs'], report['firstResultsRows'] = map(int, first.groups())
            assert report['firstResultsMs'] < 10000, report['firstResultsMs']

        conn = sqlite3.connect((service_data / 'index.db').as_uri() + '?mode=ro', uri=True, timeout=1)
        conn.execute('PRAGMA query_only=ON')
        report['queryPlan'] = conn.execute('EXPLAIN QUERY PLAN SELECT path,name,dir,is_dir,ext,size,mtime FROM entries ORDER BY name COLLATE NOCASE LIMIT 500').fetchall()
        report['meta'] = dict(conn.execute("SELECT key,value FROM meta WHERE key IN ('cached_entries_count','index_complete','last_run_id')").fetchall())
        report['dbBytes'] = (service_data / 'index.db').stat().st_size
        conn.close()
        assert any('idx_entries_name_nocase' in row[3] for row in report['queryPlan']), report['queryPlan']
        assert not any('TEMP B-TREE' in row[3] for row in report['queryPlan']), report['queryPlan']

        if toggle_log:
            before = service_status()
            toggle(False)
            time.sleep(1)
            sizes = (log_size(gui_log), log_size(service_log))
            driver.wait_query('Everything')
            time.sleep(4)
            after = service_status()
            assert sizes == (log_size(gui_log), log_size(service_log)), 'Log off must silence both processes'
            report['logOff'] = dict(guiBytes=sizes[0], serviceBytes=sizes[1], before=before, after=after)
            if require_indexing:
                assert before != after, 'The service must still be progressing while its log is off'
            toggle(True)
            driver.wait_query('Everything.exe')
            wait_for(lambda: log_size(gui_log) > sizes[0])
            if require_indexing:
                wait_for(lambda: log_size(service_log) > sizes[1])
            report['logOn'] = dict(guiBytes=log_size(gui_log), serviceBytes=log_size(service_log),
                                   setting=settings.read_text(encoding='utf-8-sig'))

        deadline = time.monotonic() + observe_seconds
        while time.monotonic() < deadline:
            service, _ = snapshot()
            print(json.dumps(dict(state=service['state'], phase=service.get('message'),
                                 scanned=service['scanned'], indexed=service['indexed'])), flush=True)
            if service['state'] == 'Ready':
                break
            time.sleep(2)
        driver.activate()
        snapshot()
        driver.capture(output / 'latest-status.png')
        report['passed'] = True
    except Exception as error:
        report['passed'] = False
        report['error'] = repr(error)
        if driver:
            try:
                driver.capture(output / 'failure.png')
            except Exception:
                pass
        raise
    finally:
        close()
        for name, path in [('gui.log', gui_log), ('service.log', service_log)]:
            if path.exists():
                (output / name).write_bytes(path.read_bytes())
        (output / 'live-results.json').write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
    print(json.dumps({key: report.get(key) for key in ['passed', 'firstResultsMs', 'firstResultsRows', 'queryPlan']}), flush=True)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--toggle-log', action='store_true')
    parser.add_argument('--require-indexing', action='store_true')
    parser.add_argument('--observe-seconds', type=int, default=30)
    args = parser.parse_args()
    run(args.binary, args.output, args.toggle_log, args.require_indexing, args.observe_seconds)
