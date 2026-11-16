#!/usr/bin/env python3
"""Exercise file-tree, settings and zoom changes in a real Windows GPUI window."""

import argparse
import ctypes
import hashlib
import json
from pathlib import Path
import time
import traceback

from run_gpui_windows_tests import App, ROOT
from windows_ui_driver import u


def run(binary, fixture, output):
    fixture, output = fixture.resolve(), output.resolve()
    if not (fixture/'.everything-ui-fixture').is_file():
        raise RuntimeError('A marked, isolated UI fixture is required')
    output.mkdir(parents=True, exist_ok=True)
    files = fixture/'files'
    prefs = fixture/'data/gpui-preferences.json'
    extras = [files/f'win-ui-tc-{i:03}.txt' for i in range(160)]
    extras += [files/'.win-ui-tc-hidden', files/'win-ui-tc-unindexed.md',
               files/'win-ui-tc-folder/empty/.keep', files/'win-ui-tc-folder/deep/leaf.txt']
    deep = []
    folder = files/'win-ui-tc-folder'
    for i in range(24):
        folder = folder/f'level-{i:02}'
        deep.append(folder)
    extras.append(folder/'deep-leaf.txt')
    refresh_file = files/'aaa-win-ui-refresh.txt'
    saved = {p: p.read_bytes() if p.is_file() else None for p in extras+[refresh_file]}
    hidden = files/'.win-ui-tc-hidden'
    file_api = ctypes.WinDLL('kernel32', use_last_error=True)
    file_api.GetFileAttributesW.argtypes = [ctypes.c_wchar_p]
    file_api.GetFileAttributesW.restype = ctypes.c_uint32
    file_api.SetFileAttributesW.argtypes = [ctypes.c_wchar_p, ctypes.c_uint32]
    hidden_attributes = file_api.GetFileAttributesW(str(hidden))
    created_dirs = set()
    for path in extras:
        directory = path.parent
        while not directory.exists():
            created_dirs.add(directory)
            directory = directory.parent
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('Isolated Windows feature fixture\n', encoding='utf-8')
    assert file_api.SetFileAttributesW(str(hidden), 2), 'Cannot create a Windows hidden-file fixture'
    prefs.write_text(json.dumps(dict(window=[100,100,1100,700], columns=[0,0,0,0],
                                    fileTreeVisible=False, fileTreeWidth=280, zoom=1)), encoding='utf-8')
    cases = []
    app = None
    active_theme = 'dark'

    def launch(theme):
        return App(binary, fixture, output, EVERYTHING_UI_TEST_THEME=theme)

    def capture(name):
        d.capture(output/f'{name}.png')

    def case(identifier, title, operation):
        started = time.monotonic()
        try:
            operation()
            cases.append(dict(id=identifier, title=title, status='pass', elapsedMs=round((time.monotonic()-started)*1000,1)))
            print(f'PASS {identifier}: {title}', flush=True)
        except Exception as error:
            cases.append(dict(id=identifier, title=title, status='fail', error=str(error), traceback=traceback.format_exc()))
            print(f'FAIL {identifier}: {str(error)[:400]}', flush=True)
            try:
                capture(identifier+'-failure')
                d.key('escape'); d.key('escape'); d.key('ctrl-0'); d.key('escape')
            except Exception:
                pass

    def row_y(index=0):
        s=d.state()
        return s['resultsTop']+(index+.5)*s.get('measuredRowHeight',s['rowHeight'])

    def query(text):
        s=d.wait_query(text)
        assert s['error'] is None, s
        return s

    def select(text, name=None):
        s=query(text)
        index=next(i for i,r in enumerate(s['rows']) if name is None or r['name']==name)
        expected=s['rows'][index]['path']
        d.click(75, row_y(index))
        return d.wait(lambda s: s['selected']==[expected] and not s['fileTree']['loading'])

    def tree_visible(visible):
        s=d.state()
        if s['fileTree']['visible'] != visible:
            d.click(s['width']-160,19)
        return d.wait(lambda s: s['fileTree']['visible']==visible)

    def revealed(path):
        return d.wait(lambda s: s['fileTree']['selected']==str(path)
                      and not s['fileTree']['loading']
                      and any(r.get('path')==str(path) for r in s['fileTree']['rows']))

    def tree_click(name):
        s=d.wait(lambda s: any(r.get('name')==name for r in s['fileTree']['rows']))
        tree=s['fileTree']
        row=next(r for r in tree['rows'] if r.get('name')==name)
        x,y,width,height=tree['listBounds']
        click_y=y+(row['index']+.5)*tree.get('rowHeight',s['rowHeight'])+tree['scrollOffset']
        assert y<=click_y<y+height, row
        d.click(x+min(width-15,(55+row['depth']*14)*s['zoom']),click_y)
        return row

    def tree_header(offset):
        s=d.state(); tree=s['fileTree']
        d.click(s['width']-offset*s['zoom'],tree['listBounds'][1]-(26+12.5)*s['zoom'])

    def zoom(key,expected):
        d.key(key)
        return d.wait(lambda s: abs(s['zoom']-expected)<.005
                      and abs(s['rowHeight']-27*expected)<.1
                      and (s['modal'] is not None or abs(s['resultsTop']-(38+69*expected))<1))

    def zoom_toggle():
        s=d.state(); x,y,w,h=s['zoomPopover']['buttonBounds']
        assert s['zoomPopover']['buttonVisible']
        d.click(x+w/2,y+h/2)
        return d.wait(lambda after: after['zoomPopover']['visible']!=s['zoomPopover']['visible'])

    def zoom_control(index,expected):
        s=d.state(); x,y,w,h=s['zoomPopover']['bounds']
        assert s['zoomPopover']['visible']
        d.click(x+w-[135,99,45][index],y+h/2)
        return d.wait(lambda s: abs(s['zoom']-expected)<.005)

    try:
        app=launch(active_theme); d=app.driver
        d.wait(lambda s: s['theme']=='dark' and not s['busy'] and s['resultsTop']>0)

        def settings():
            s=d.state(); d.click(s['width']-29,s['height']-16)
            d.wait(lambda s: s['modal']=='settings' and s['modalSelection']==0)
            capture('01-settings-dark')
            d.key('down'); d.wait(lambda s: s['modalSelection']==1)
            d.key('down'); d.wait(lambda s: s['modalSelection']==2)
            d.key('enter')
            d.wait(lambda s: s['modal'] is None and 'disabled in isolated' in (s['toast'] or ''))
        case('NEW-01','Windows settings keyboard navigation and isolated Reset guard',settings)

        def toggle_and_follow():
            before=d.bounds()
            tree_visible(True)
            assert d.bounds()==before
            select('Alpha','Alpha report.txt')
            s=revealed(files/'Alpha report.txt')
            assert s['fileTree']['root']==str(files),s
            assert s['fileTree']['width']==280,s
            capture('02-tree-dark')
        case('NEW-02','Titlebar file-tree toggle and selected-result follow without moving the window',toggle_and_follow)

        def all_siblings():
            tree_click('Alpha report.txt'); d.key('home')
            d.wait(lambda s: s['fileTree']['selectedIndex']==0 and s['fileTree']['visibleRange'][0]==0)
            seen=set()
            for _ in range(40):
                s=d.state(); tree=s['fileTree']
                seen.update(r['name'] for r in tree['rows'] if 'path' in r)
                if tree['selectedIndex']==tree['rowCount']-1: break
                previous=tree['selectedIndex']; d.key('pagedown')
                d.wait(lambda s: s['fileTree']['selectedIndex']!=previous
                       and s['fileTree']['visibleRange'][0]<=s['fileTree']['selectedIndex']<s['fileTree']['visibleRange'][1])
            expected={p.name for p in files.iterdir()}|{files.name}
            assert seen==expected,dict(missing=sorted(expected-seen),extra=sorted(seen-expected))
            assert d.state()['selected']==[str(files/'Alpha report.txt')]
        case('NEW-03','Virtualized tree includes hidden/unindexed siblings and keeps result selection independent',all_siblings)

        def nested_tree():
            select('Alpha','Alpha report.txt'); revealed(files/'Alpha report.txt')
            tree_click('win-ui-tc-folder')
            d.wait(lambda s: any(r.get('name')=='level-00' for r in s['fileTree']['rows']))
            tree_click('level-00'); d.wait(lambda s: not s['fileTree']['loading'])
            for directory in deep[1:]:
                d.key('right'); revealed(directory)
                d.key('right'); d.wait(lambda s: not s['fileTree']['loading'])
            d.key('right'); revealed(deep[-1]/'deep-leaf.txt')
            assert not d.state()['fileTree']['selectionError']
            capture('03-deep-tree')
            # 0.6.4 adds file actions to the tree. They must target the tree
            # selection while preserving the independently selected result.
            leaf=deep[-1]/'deep-leaf.txt'
            assert leaf.is_file()
            d.key('shift-delete')
            d.wait(lambda s: not s['actionBusy'] and s['completedAction']=='DeletePermanently' and not leaf.exists())
            assert (files/'Alpha report.txt').is_file(), 'Tree actions must preserve the selected table result'
        case('NEW-04','24-level folder navigation and 0.6.4 tree Shift+Delete targets only the tree selection',nested_tree)

        def refresh_and_copy():
            select('한글'); s=revealed(files/'한글 보고서.txt')
            before=s['fileTree']
            refresh_file.write_text('New unindexed entry\n',encoding='utf-8')
            tree_header(21)
            increase=1 if saved[refresh_file] is None else 0
            d.wait(lambda s: not s['fileTree']['loading'] and s['fileTree']['rowCount']==before['rowCount']+increase)
            refreshed=revealed(files/'한글 보고서.txt')
            if increase:
                assert abs(refreshed['fileTree']['scrollOffset']-before['scrollOffset']
                           +before.get('rowHeight',s['rowHeight']))<1, dict(
                    before=before['scrollOffset'], after=refreshed['fileTree']['scrollOffset'],
                    rowHeight=before.get('rowHeight',s['rowHeight']))
            s=d.state(); d.click(30,s['height']-16)
            time.sleep(.15)  # Let the UI finish writing before opening the Windows clipboard.
            d.wait(lambda _: d.clipboard()['text']==str(files/'한글 보고서.txt'))
            s=d.state(); tree=s['fileTree']
            d.click(tree['listBounds'][0]+20,tree['listBounds'][1]-13)
            time.sleep(.15)
            d.wait(lambda _: d.clipboard()['text']==str(files))
            assert d.state()['selected']==[str(files/'한글 보고서.txt')]
            before=d.state()['fileTree']['scrollOffset']
            d.send('hide'); time.sleep(.15); d.send('activate')
            s=revealed(files/'한글 보고서.txt')
            assert abs(s['fileTree']['scrollOffset']-before)<1
        case('NEW-05','Refresh and footer/tree path clicks copy full Unicode paths through middle ellipsis',refresh_and_copy)

        def tree_resize_parent():
            select('Alpha','Alpha report.txt'); revealed(files/'Alpha report.txt')
            s=d.state(); width=s['fileTree']['width']; edge=s['width']-width; before=d.bounds()
            d.send('down',x=edge,y=19); d.send('move',x=edge-45,y=19); d.send('up',x=edge-45,y=19)
            d.wait(lambda s: s['fileTree']['width']>=width+40)
            assert d.bounds()==before
            tree_header(47)
            d.wait(lambda s: s['fileTree']['root']==str(fixture))
            revealed(files/'Alpha report.txt')
        case('NEW-06','Windows titlebar divider resizes only the tree; parent navigation retains the selected file',tree_resize_parent)

        def unavailable():
            select('report_0000000.txt')
            d.wait(lambda s: s['fileTree']['selectionError'] is not None)
            select('Alpha','Alpha report.txt'); revealed(files/'Alpha report.txt')
            assert d.state()['fileTree']['selectionError'] is None
        case('NEW-07','Unavailable directories show an error and recover on selecting a real file',unavailable)

        def zoom_shortcuts():
            query('Alpha'); before=d.bounds()
            for key,expected in [('ctrl-equals',1.1),('ctrl-shift-plus',1.25),('ctrl-minus',1.1),('ctrl-0',1.)]:
                s=zoom(key,expected)
                assert s['query']=='Alpha' and s['zoomPopover']['visible']
                assert d.bounds()==before
            d.key('escape')
            d.key('plus'); d.wait(lambda s: s['query']=='Alpha+' and s['zoom']==1.)
            d.key('minus'); d.wait(lambda s: s['query']=='Alpha+-' and s['zoom']==1.)
            select('한글'); revealed(files/'한글 보고서.txt'); tree_click('한글 보고서.txt')
            d.key('pageup')
            s=d.wait(lambda s: s['fileTree']['focused']
                     and s['fileTree']['visibleRange'][0]<=s['fileTree']['selectedIndex']<s['fileTree']['visibleRange'][1])
            selected=s['fileTree']['selected']; top=s['fileTree']['visibleRange'][0]
            s=zoom('ctrl-plus',1.1)
            d.wait(lambda s: s['fileTree']['focused'] and s['fileTree']['selected']==selected
                   and s['fileTree']['visibleRange'][0]<=s['fileTree']['selectedIndex']<s['fileTree']['visibleRange'][1])
            assert abs(d.state()['fileTree']['visibleRange'][0]-top)<=1
            zoom('ctrl-0',1.); d.key('escape')
        case('NEW-08','Windows zoom chords/literal plus-minus, fixed window bounds, and tree-focused scroll/selection preservation',zoom_shortcuts)

        def zoom_selection_rename():
            select('gpui-tc-rename.txt'); original=d.state()['selected']
            d.key('f2'); before=d.wait(lambda s: s['renamePath'] is not None)
            s=zoom('ctrl-plus',1.1)
            assert s['renamePath']==before['renamePath'] and s['renameValue']==before['renameValue']
            zoom('ctrl-0',1.); d.key('escape'); d.key('escape')
            s=d.wait(lambda s: s['renamePath'] is None)
            assert s['selected']==original
        case('NEW-09','Windows F2 rename and selection survive zoom without committing a filename',zoom_selection_rename)

        def zoom_mouse_keyboard():
            zoom('ctrl-plus',1.1); d.key('escape'); zoom_toggle()
            assert d.state()['zoomPopover']['pinned']
            zoom_control(1,1.25); zoom_control(1,1.5); zoom_control(0,1.25)
            zoom_control(2,1.); zoom_control(2,1.)
            assert d.state()['zoomPopover']['buttonVisible']
            d.key('tab'); d.wait(lambda s: s['zoomPopover']['keyboardSelection']==0)
            d.key('right'); d.key('enter'); d.wait(lambda s: abs(s['zoom']-1.1)<.005)
            d.key('tab'); d.key('space'); d.wait(lambda s: s['zoom']==1.)
            d.key('escape'); assert not d.state()['zoomPopover']['visible']
            capture('04-zoom-controls')
        case('NEW-10','Zoom icon, mouse controls, disabled reset and Tab/arrow/Enter/Space/Escape navigation',zoom_mouse_keyboard)

        def zoom_timers():
            d.send('move',x=40,y=200); zoom('ctrl-plus',1.1)
            time.sleep(1.7); zoom('ctrl-plus',1.25); time.sleep(1.7)
            assert d.state()['zoomPopover']['visible']
            d.wait(lambda s: not s['zoomPopover']['visible'],timeout=5)
            s=zoom('ctrl-plus',1.5); x,y,w,h=s['zoomPopover']['bounds']
            d.send('move',x=x+20,y=y+h/2)
            d.wait(lambda s: s['zoomPopover']['hovered']); time.sleep(3.2)
            assert d.state()['zoomPopover']['visible']
            d.send('move',x=40,y=200); d.wait(lambda s: not s['zoomPopover']['visible'],timeout=5)
            zoom('ctrl-0',1.); d.key('escape')
        case('NEW-11','Zoom popover timer resets on shortcuts, pauses while hovered and closes after leaving',zoom_timers)

        def limits_small():
            for _ in range(20): d.key('ctrl-plus')
            d.wait(lambda s: s['zoom']==2.)
            d.resize(400,300); s=d.wait(lambda s: s['width']==400 and s['height']==300)
            x,y,w,h=s['zoomPopover']['bounds']
            assert w==260 and h==52 and x>=8 and x+w<=392,s
            capture('05-small-zoom-200-dark')
            d.key('escape'); s=d.state()
            d.click(s['width']-29*2,s['height']-16*2)
            d.wait(lambda s: s['modal']=='settings'); capture('06-small-settings-dark'); d.key('escape')
            before=d.state()['fileTree']['visible']
            d.click(400-160,19)
            d.wait(lambda s: s['fileTree']['visible']!=before)
            for _ in range(30): d.key('ctrl-minus')
            d.wait(lambda s: s['zoom']==.5)
            zoom('ctrl-0',1.); d.key('escape'); d.resize(1100,700)
            d.wait(lambda s: s['width']==1100 and s['height']==700)
        case('NEW-12','50–200% bounds, 400x300 settings/zoom hit targets and adjacent Windows tree toggle',limits_small)

        def persistence_light():
            nonlocal app,d,active_theme
            tree_visible(True); zoom('ctrl-plus',1.1); zoom('ctrl-plus',1.25)
            before=d.state(); time.sleep(.4); app.close()
            active_theme='light'; app=launch(active_theme); d=app.driver
            s=d.wait(lambda s: s['theme']=='light' and s['zoom']==1.25 and not s['busy'])
            assert s['fileTree']['visible'] and abs(s['fileTree']['width']-before['fileTree']['width'])<1
            assert (s['width'],s['height'])==(before['width'],before['height'])
            select('한글'); revealed(files/'한글 보고서.txt')
            capture('07-light-tree-zoom-125')
            s=d.state(); d.click(s['width']-29*s['zoom'],s['height']-16*s['zoom'])
            d.wait(lambda s: s['modal']=='settings'); capture('08-light-settings')
            zoom('ctrl-plus',1.5)
            assert d.state()['modal']=='settings'; capture('09-light-settings-zoom')
            d.key('escape'); d.key('escape'); zoom('ctrl-0',1.); d.key('escape')
        case('NEW-13','Saved zoom/tree proportions and light-palette Geist/Lucide rendering; zoom while settings are open',persistence_light)

        def light_small_titlebar():
            zoom('ctrl-plus',1.1); zoom('ctrl-plus',1.25); zoom('ctrl-plus',1.5)
            zoom('ctrl-plus',1.75); zoom('ctrl-plus',2.)
            d.resize(400,300); s=d.wait(lambda s: s['width']==400 and s['height']==300)
            capture('10-light-small-200')
            d.key('escape'); d.resize(1100,700); zoom('ctrl-0',1.); d.key('escape')
            before=d.bounds(); d.drag(60,19,60,35); after=d.bounds()
            assert after['x']>before['x'] and after['y']>before['y']
            capture('11-light-titlebar')
        case('NEW-14','Light 200% small-window layout and native caption dragging after all new controls',light_small_titlebar)
    finally:
        if app: app.close()
        for path,content in saved.items():
            if content is None: path.unlink(missing_ok=True)
            else: path.write_bytes(content)
        if saved[hidden] is not None:
            assert file_api.SetFileAttributesW(str(hidden), hidden_attributes)
        for directory in sorted(created_dirs,key=lambda p:len(p.parts),reverse=True):
            if directory.exists(): directory.rmdir()
    report=dict(binary=str(binary),sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                date=time.strftime('%Y-%m-%dT%H:%M:%S%z'),passed=sum(c['status']=='pass' for c in cases),
                failed=sum(c['status']=='fail' for c in cases),cases=cases)
    (output/'feature-results.json').write_text(json.dumps(report,ensure_ascii=False,indent=2),encoding='utf-8')
    print(json.dumps(dict(passed=report['passed'],failed=report['failed'])),flush=True)
    return report


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--binary',type=Path,default=ROOT/'app/target/release/everything.exe')
    p.add_argument('--fixture',type=Path,required=True)
    p.add_argument('--output',type=Path,default=ROOT/'.artifacts/windows-ui-features')
    a=p.parse_args(); report=run(a.binary.resolve(),a.fixture,a.output)
    raise SystemExit(bool(report['failed']))
