#!/usr/bin/env python3
"""Exercise a release GPUI window using native AppKit events, never model writes.

Requires a binary built with --features ui-automation and an isolated fixture
created by ui_fixture.py. All filesystem mutations stay under that fixture.
"""
import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import time
import traceback

from ui_driver import Driver

ROOT = Path(__file__).resolve().parents[1]


class App:
    def __init__(self, binary, fixture, work, **extra):
        self.socket = work / f'ui-{time.time_ns()}.sock'
        self.state_file = work / f'state-{time.time_ns()}.json'
        env = dict(os.environ)
        for key in ['EVERYTHING_UI_TEST_ONBOARDING', 'EVERYTHING_UI_BENCH_PAGE_SIZE']:
            env.pop(key, None)
        env.update(EVERYTHING_UI_TEST='1', EVERYTHING_DATA_DIR=str(fixture / 'data'),
                   EVERYTHING_UI_STATE=str(self.state_file),
                   EVERYTHING_UI_AUTOMATION_SOCKET=str(self.socket), **extra)
        self.log = (work / f'app-{time.time_ns()}.log').open('w')
        self.process = subprocess.Popen([str(binary)], env=env, stdout=self.log, stderr=subprocess.STDOUT)
        self.driver = Driver(self.socket, self.state_file)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError(f'App exited {self.process.returncode}: {self.log.name}')
            if self.socket.exists() and self.state_file.exists():
                try:
                    self.driver.send('window')
                    self.driver.state()
                    break
                except (OSError, ValueError):
                    pass
            time.sleep(0.025)
        else:
            self.close()
            raise TimeoutError('Native test window did not start')
        time.sleep(0.1)

    def close(self):
        if self.process.poll() is None:
            try:
                self.driver.key('cmd-q')
            except (OSError, ValueError):
                pass
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate()
                self.process.wait(timeout=5)
        self.log.close()
        self.socket.unlink(missing_ok=True)


def run(binary, fixture, output):
    fixture = fixture.resolve()
    if not (fixture / '.everything-ui-fixture').is_file():
        raise RuntimeError('Refusing UI mutations outside a marked fixture')
    output.mkdir(parents=True, exist_ok=True)
    results = []
    prefs_file = fixture / 'data/gpui-preferences.json'
    prefs_file.write_text(json.dumps(dict(theme='system', columns=[0, 0, 0, 0], window=[100, 100, 900, 560], fdaDismissed=False)))
    files = fixture / 'files'
    # Dedicated test files can be restored between repeated isolated runs.
    originals = {name: (files / name).read_bytes() for name in ['gpui-tc-rename.txt', 'gpui-tc-trash.txt']}
    app = None
    with tempfile.TemporaryDirectory(prefix='ev-gpui-tc-') as temporary:
        work = Path(temporary)
        app = App(binary, fixture, work)
        d = app.driver
        d.send('appearance', theme='dark')
        d.wait(lambda s:s['theme']=='dark')

        def capture(name):
            time.sleep(0.08)
            d.send('capture', path=str((output / f'{name}.png').resolve()))

        def case(identifier, title, fn):
            started = time.monotonic()
            try:
                fn()
                results.append(dict(id=identifier, title=title, status='pass', elapsedMs=(time.monotonic()-started)*1000))
                print(f'PASS {identifier}: {title}', flush=True)
            except Exception as error:
                results.append(dict(id=identifier, title=title, status='fail', error=str(error), traceback=traceback.format_exc()))
                print(f'FAIL {identifier}: {error}', flush=True)
                try:
                    d.send('menu_key',key='escape')
                    capture(f'{identifier}-failure')
                    d.key('escape')
                    d.key('escape')
                except Exception:
                    pass

        def query(text, count=None, loaded=None):
            s = d.wait_query(text)
            if count is not None:
                assert s['total'] == count, s
            if loaded is not None:
                assert s['loaded'] == loaded, s
            assert s['error'] is None, s
            return s

        def idle():
            return d.wait(lambda s: not s['actionBusy'])

        def selected_one(text):
            query(text, 1)
            d.key('down')
            return d.wait(lambda s: len(s['selected']) == 1)

        try:
            def empty_and_focus():
                d.wait(lambda s:s['query']=='' and not s['busy'] and s['loaded']==500)
                d.text('한글')
                s=d.wait(lambda s: s['query']=='한글' and not s['busy'])
                assert [r['name'] for r in s['rows']]==['한글 보고서.txt'],s
                query('',500018,500)
                capture('01-empty')
            case('UI-01','Startup focus, Korean committed text, clear input',empty_and_focus)

            def search_grammar():
                for text,count in [('Alpha',2),('ALPHA',2),('space name',1),('한글',1),('*.md',62502),('no-such-gpui-file-xyz',0)]:
                    query(text,count)
                s=query('folder-demo/',1)
                assert s['rows'][0]['name']=='child.txt',s
                s=query(str(files/'folder-demo')+'/',1)
                assert s['rows'][0]['name']=='child.txt',s
                capture('02-path')
                query('report',500001,500)
                capture('03-results')
            case('UI-02','Name/case/space/Korean/glob/relative and absolute path/empty result',search_grammar)

            def stale_and_short():
                for text in ['report','Alpha','*.md','한글']:
                    d.query(text)
                s=d.wait(lambda s:s['query']=='한글' and not s['busy'])
                assert s['loaded']==1 and s['rows'][0]['name']=='한글 보고서.txt',s
                s=query('r',loaded=100)
                assert len(s['rows'])==35,s
            case('UI-03','Rapid replacement rejects stale results; single character cap',stale_and_short)

            def pagination():
                query('report',loaded=500)
                d.key('down')
                for _ in range(40):
                    d.key('pagedown')
                    if d.state()['loaded']>=1000:
                        break
                s=d.wait(lambda s:s['loaded']>=1000 and not s['paging'])
                assert s['primary']>=200,s
                capture('04-pagination')
            case('UI-04','Actual keyboard scrolling and next-page load',pagination)

            def sort_and_resize():
                query('gpui-tc-',4)
                d.click(70,90)
                s=d.wait(lambda s:s['sortBy']=='name' and s['sortDir']=='desc' and not s['busy'])
                assert s['rows'][0]['name']=='gpui-tc-trash.txt',s
                d.click(735,90)
                s=d.wait(lambda s:s['sortBy']=='size' and not s['busy'])
                assert [r['size'] for r in s['rows']]==sorted(r['size'] for r in s['rows']),s
                d.click(70,90)
                d.wait(lambda s:s['sortBy']=='name' and s['sortDir']=='asc' and not s['busy'])
                width=d.state()['columns'][0]
                d.send('down',x=width-2,y=90)
                d.send('move',x=width+78,y=90,drag=True)
                d.send('up',x=width+78,y=90)
                s=d.wait(lambda s:s['columns'][0]>=width+70)
                assert s['sortDir']=='asc',s
                capture('05-columns')
            case('UI-05','Server sort, header drag resize, sort/resize hit testing',sort_and_resize)

            def selection_clipboard():
                query('gpui-tc-',4)
                d.click(90,120)
                d.click(90,174,modifiers=['cmd'])
                s=d.wait(lambda s:len(s['selected'])==2)
                assert s['selected'][0].endswith('gpui-tc-collision.txt'),s
                d.click(90,201,modifiers=['shift'])
                d.wait(lambda s:len(s['selected'])==2)
                d.key('shift-up')
                d.key('cmd-a')
                d.wait(lambda s:len(s['selected'])==4)
                d.key('alt-cmd-c')
                d.wait(lambda s:not s['actionBusy'] and (s['toast'] or '').startswith('Copied 4 path'))
                board=d.send('clipboard')
                assert len(board['text'].splitlines())==4,board
                d.key('cmd-c')
                d.wait(lambda s:not s['actionBusy'] and (s['toast'] or '').startswith('Copied 4 item'))
                board=d.send('clipboard')
                assert 'public.file-url' in board['types'],board
                d.key('escape')
                assert not d.state()['selected']
                capture('06-selection')
            case('UI-06','Mouse/keyboard multi-selection and real path/file pasteboard',selection_clipboard)

            def menu():
                selected_one('gpui-tc-open.txt')
                capture('07-context-selection')
                d.click(100,120,button='right')
                d.wait(lambda s:s['contextMenu'])
                time.sleep(.15)
                for _ in range(6):
                    d.send('menu_key',key='down')
                    time.sleep(.04)
                d.send('menu_key',key='enter')
                d.wait(lambda s:not s['actionBusy'] and not s['contextMenu'] and (s['toast'] or '').startswith('Copied 1 path'))
                assert d.send('clipboard')['text']==str(files/'gpui-tc-open.txt')
            case('UI-07','Context menu keyboard navigation and Copy Path',menu)

            def rename():
                selected_one('gpui-tc-rename.txt')
                d.key('f2')
                d.wait(lambda s:s['renamePath'] is not None)
                d.text('gpui-tc-renamed')
                s=d.wait(lambda s:s['renameValue']=='gpui-tc-renamed.txt')
                d.key('enter')
                d.wait(lambda s:not s['actionBusy'] and s['renamePath'] is None)
                assert not (files/'gpui-tc-rename.txt').exists()
                assert (files/'gpui-tc-renamed.txt').read_bytes()==originals['gpui-tc-rename.txt']
                d.key('f2')
                d.key('cmd-a')
                d.text('gpui-tc-collision.txt')
                d.key('enter')
                s=d.wait(lambda s:not s['actionBusy'] and (s['toast'] or '')!='Renamed')
                assert s['renamePath'] is not None,s
                capture('08-rename-collision')
                d.key('cmd-a');d.key('backspace');d.key('enter')
                d.wait(lambda s:s['renameValue']=='' and not s['actionBusy'])
                assert d.state()['renamePath'] is not None
                d.key('cmd-a');d.text('bad/name.txt');d.key('enter')
                d.wait(lambda s:s['renameValue']=='bad/name.txt' and not s['actionBusy'])
                assert d.state()['renamePath'] is not None
                d.key('cmd-a');d.text('gpui-tc-rename.txt');d.key('enter')
                d.wait(lambda s:not s['actionBusy'] and s['renamePath'] is None)
                assert (files/'gpui-tc-rename.txt').exists()
                assert not (files/'gpui-tc-renamed.txt').exists()
                d.key('f2')
                d.wait(lambda s:s['renamePath'] is not None)
                d.key('cmd-a');d.text('gpui-tc-renamed.txt')
                d.click(80,50)
                d.wait(lambda s:not s['actionBusy'] and s['renamePath'] is None)
                assert (files/'gpui-tc-renamed.txt').exists()
                # Restore via the still-rendered row, since fixture DB writes are disabled.
                d.click(90,120);d.key('f2')
                d.wait(lambda s:s['renamePath'] is not None)
                d.key('cmd-a');d.text('gpui-tc-rename.txt');d.key('enter')
                d.wait(lambda s:not s['actionBusy'] and s['renamePath'] is None)
                assert (files/'gpui-tc-rename.txt').exists()
            case('UI-08','F2 Rename, stem selection, collision/empty/slash errors, success',rename)

            def trash():
                selected_one('gpui-tc-trash.txt')
                d.key('cmd-backspace')
                d.wait(lambda s:s['modal']=='trash')
                d.key('enter')
                d.wait(lambda s:s['modal'] is None)
                assert (files/'gpui-tc-trash.txt').exists()
                d.key('delete')
                d.wait(lambda s:s['modal']=='trash')
                capture('09-trash-confirmation')
                d.key('tab');d.key('enter')
                s=d.wait(lambda s:s['modal'] is None and not s['actionBusy'] and s['loaded']==0)
                assert not (files/'gpui-tc-trash.txt').exists(),s
                query('gpui-tc-',3,3)
                assert all(r['name']!='gpui-tc-trash.txt' for r in d.state()['rows'])
            case('UI-09','Safe default Trash cancel, keyboard confirm, real filesystem removal and suppression',trash)

            def themes_and_dialog():
                for theme in ['light','dark']:
                    d.send('appearance',theme=theme)
                    d.wait(lambda s:s['theme']==theme)
                    capture(f'10-theme-{theme}')
                s=d.state()
                d.click(s['width']-48,s['height']-14)
                d.wait(lambda s:'disabled in isolated' in (s['toast'] or ''))
                assert d.state()['modal'] is None
                d.send('resize',width=400,height=300)
                d.wait(lambda s:s['width']==400 and s['height']==300)
                selected_one('gpui-tc-open.txt')
                d.key('delete')
                d.wait(lambda s:s['modal']=='trash')
                capture('11-small-window-dialog')
                d.key('escape')
                d.wait(lambda s:s['modal'] is None)
                d.send('resize',width=1010,height=610)
                d.wait(lambda s:s['width']==1010 and s['height']==610)
            case('UI-10','System light/dark, isolated Reset guard, minimum size and responsive confirmation',themes_and_dialog)

            def open_reveal_preview():
                selected_one('gpui-tc-open.txt')
                for key,action in [('enter','Open'),('cmd-enter','Reveal'),('cmd-o','Open'),('space','QuickLook')]:
                    d.key(key)
                    d.wait(lambda s:not s['actionBusy'] and s['completedAction']==action)
                # Only terminate the Quick Look process launched for this exact fixture path.
                for line in subprocess.check_output(['ps','-axo','pid=,command='],text=True).splitlines():
                    parts=line.strip().split(None,1)
                    if len(parts)==2 and parts[1].startswith('qlmanage -p ') and str(files/'gpui-tc-open.txt') in parts[1]:
                        os.kill(int(parts[0]),15)
            case('UI-11','OS Open/Reveal/Quick Look requests on a fixture file',open_reveal_preview)

            def persistence():
                nonlocal app,d
                d.send('resize',width=1010,height=610)
                d.wait(lambda s:s['width']==1010 and s['height']==610)
                before=d.state()
                app.close()
                saved=json.loads(prefs_file.read_text())
                assert saved['theme']=='system',saved
                assert saved['window'][2:]==[1010,610],saved
                app=App(binary,fixture,work);d=app.driver
                s=d.state()
                assert s['columns']==before['columns'],s
                assert s['width']==1010 and s['height']==610,s
                app.close();app=App(binary,fixture,work);d=app.driver
                s=d.state();assert s['height']==610,s
                capture('12-preferences-restart')
            case('UI-12','Column/window persistence and no size growth across two restarts',persistence)

            def onboarding():
                nonlocal app,d
                app.close();app=App(binary,fixture,work,EVERYTHING_UI_TEST_ONBOARDING='1');d=app.driver
                d.wait(lambda s:s['modal']=='onboarding')
                d.key('escape')
                assert d.state()['modal']=='onboarding'
                capture('13-onboarding')
                d.key('enter')
                s=d.wait(lambda s:s['modal'] is None)
                assert s['fdaOk'] is False,s
                d.click(s['width']-100,45)
                d.wait(lambda s:s['fdaSettingsOpened'])
                capture('13-fda-banner')
                # The banner has its own close button; user defaults are never written.
                d.click(s['width']-30,45)
                d.wait(lambda s:s['fdaDismissed'])
            case('UI-13','First-run modal keyboard trap, Later choice and dismissible FDA banner',onboarding)
        finally:
            if app:app.close()
            for name,data in originals.items():
                if not (files/name).exists():(files/name).write_bytes(data)
            if (files/'gpui-tc-renamed.txt').exists():
                (files/'gpui-tc-renamed.txt').unlink()
    report=dict(binary=str(binary),fixture=str(fixture),date=time.strftime('%Y-%m-%dT%H:%M:%S%z'),
                passed=sum(c['status']=='pass' for c in results),failed=sum(c['status']=='fail' for c in results),cases=results)
    (output/'ui-results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
    return report


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary',type=Path,default=ROOT/'src-tauri/target/release/everything')
    p.add_argument('--fixture',type=Path,required=True)
    p.add_argument('--output',type=Path,default=ROOT/'.artifacts/gpui-ui')
    a=p.parse_args();report=run(a.binary.resolve(),a.fixture,a.output)
    print(json.dumps(dict(passed=report['passed'],failed=report['failed'])))
    raise SystemExit(bool(report['failed']))
