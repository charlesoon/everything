#!/usr/bin/env python3
"""Verify the right file tree through real GPUI/AppKit input in a marked fixture."""

import argparse
import json
from pathlib import Path
import tempfile
import time
import traceback

from run_gpui_ui_tests import App, ROOT


def run(binary, fixture, output):
    fixture = fixture.resolve()
    if not (fixture / '.everything-ui-fixture').is_file():
        raise RuntimeError('Refusing UI mutations outside a marked fixture')
    output.mkdir(parents=True, exist_ok=True)
    files = fixture / 'files'
    prefs = fixture / 'data/gpui-preferences.json'
    prefs.write_text(json.dumps(dict(window=[100, 100, 900, 560], columns=[0, 0, 0, 0])))
    extras = [files / f'file-tree-tc-scroll-{i:03}.txt' for i in range(320)]
    extras += [files / '.file-tree-tc-hidden', files / 'file-tree-tc-unindexed.md',
               files / 'file-tree-tc-folder/deep/leaf.txt',
               files / 'folder-demo/file-tree-tc-sibling.md']
    deep_directories = []
    directory = files / 'file-tree-tc-folder'
    for level in range(24):
        directory = directory / f'level-{level:02}'
        deep_directories.append(directory)
    deep_leaf = directory / 'deep-leaf.txt'
    extras.append(deep_leaf)
    refresh_file = files / 'folder-demo/file-tree-tc-refresh.md'
    position_file = files / 'file-tree-tc-position-anchor.txt'
    originals = {path: path.read_bytes() if path.exists() else None
                 for path in extras + [refresh_file, position_file, files / 'gpui-tc-rename.txt',
                                       files / 'gpui-tc-trash.txt']}
    for path in extras:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('Unindexed file tree fixture\n')
    empty = files / 'file-tree-tc-folder/empty'
    empty.mkdir(exist_ok=True)
    results = []
    app = None

    with tempfile.TemporaryDirectory(prefix='ev-file-tree-tc-') as temporary:
        work = Path(temporary)
        app = App(binary, fixture, work)
        d = app.driver

        def capture(name):
            time.sleep(.1)
            d.send('capture', path=str((output / f'{name}.png').resolve()))

        def case(identifier, title, fn):
            started = time.monotonic()
            try:
                fn()
                results.append(dict(id=identifier, title=title, status='pass',
                                    elapsedMs=(time.monotonic() - started) * 1000))
                print(f'PASS {identifier}: {title}', flush=True)
            except Exception as error:
                results.append(dict(id=identifier, title=title, status='fail',
                                    error=str(error), traceback=traceback.format_exc()))
                print(f'FAIL {identifier}: {error}', flush=True)
                try:
                    capture(f'{identifier}-failure')
                    d.key('escape')
                except Exception:
                    pass

        def revealed(path):
            def predicate(state):
                tree = state['fileTree']
                index = tree['selectedIndex']
                return (tree['selected'] == str(path) and index is not None
                        and tree['visibleRange'][0] <= index < tree['visibleRange'][1]
                        and any(row.get('path') == str(path) for row in tree['rows']))
            return d.wait(predicate)

        def select(text, name=None):
            state = d.wait_query(text)
            index = next(i for i, row in enumerate(state['rows'])
                         if name is None or row['name'] == name)
            d.click(90, state['resultsTop'] + (index + .5) * state['rowHeight'])
            return revealed(Path(state['rows'][index]['path']))

        def tree_click(name):
            state = d.wait(lambda s: any(row.get('name') == name for row in s['fileTree']['rows']))
            tree = state['fileTree']
            row = next(row for row in tree['rows'] if row.get('name') == name)
            x, y, width, height = tree['listBounds']
            click_y = y + (row['index'] + .5) * state['rowHeight'] + tree['scrollOffset']
            assert y <= click_y < y + height, (tree, click_y)
            d.click(x + min(width - 15, 65 + row['depth'] * 14), click_y)
            return row

        def toggle():
            state = d.state()
            d.click(state['width'] - 24, 15)
            return d.wait(lambda s: s['fileTree']['visible'] != state['fileTree']['visible'])

        def tree_header_click(right_offset):
            state = d.state()
            header_y = state['fileTree']['listBounds'][1] - (26 + 33 / 2) * state['zoom']
            d.click(state['width'] - right_offset, header_y)

        def restart():
            nonlocal app, d
            app.close()
            app = App(binary, fixture, work)
            d = app.driver

        try:
            d.send('appearance', theme='dark')
            d.wait(lambda s: s['theme'] == 'dark' and not s['busy'])
            original_columns = d.state()['columns']

            def empty_toggle():
                state = d.state()
                assert not state['fileTree']['visible'], state
                before = d.send('window')
                state = toggle()
                assert state['fileTree']['root'] is None, state
                after = d.send('window')
                assert all(before[key] == after[key] for key in ['x', 'y', 'width', 'height'])
                capture('01-empty-tree')
            case('TREE-01', 'Top-right toggle opens an empty panel without moving the window', empty_toggle)

            def siblings():
                state = select('Alpha', 'Alpha report.txt')
                assert state['fileTree']['root'] == str(files), state
                # The tree header/path (33+26) sit beside the search/header
                # (44+33), so its rows start 18 design pixels earlier.
                assert abs(state['fileTree']['listBounds'][1]
                           - state['resultsTop'] + 18 * state['zoom']) < 1, state
                assert state['query'] == 'Alpha', state
                tree_click('Alpha report.txt')
                d.key('home')
                d.wait(lambda s: s['fileTree']['selectedIndex'] == 0
                       and s['fileTree']['visibleRange'][0] == 0)
                seen = set()
                for _ in range(80):
                    state = d.state()
                    tree = state['fileTree']
                    seen.update(row.get('name') for row in tree['rows'] if 'path' in row)
                    if tree['selectedIndex'] == tree['rowCount'] - 1:
                        break
                    previous = tree['selectedIndex']
                    d.key('pagedown')
                    d.wait(lambda s: s['fileTree']['selectedIndex'] != previous
                           and s['fileTree']['visibleRange'][0] <= s['fileTree']['selectedIndex'] < s['fileTree']['visibleRange'][1])
                expected = {path.name for path in files.iterdir()} | {files.name}
                assert seen == expected, dict(missing=sorted(expected - seen), extra=sorted(seen - expected))
                assert len(seen) > 320
                assert d.state()['selected'] == [str(files / 'Alpha report.txt')]
            case('TREE-02', 'Clicked result is revealed; every sibling, including hidden/unindexed files, is browsable', siblings)

            def expand_and_keyboard():
                # Clicking the already selected result must reveal it again.
                d.click(90, 120)
                revealed(files / 'Alpha report.txt')
                tree_click('file-tree-tc-folder')
                d.wait(lambda s: any(row.get('name') == 'deep' for row in s['fileTree']['rows']))
                tree_click('empty')
                d.wait(lambda s: any(row.get('message') == 'Empty folder' for row in s['fileTree']['rows']))
                d.key('left')
                d.key('left')
                d.wait(lambda s: s['fileTree']['selected'] == str(files / 'file-tree-tc-folder'))
                d.key('delete')
                assert d.state()['modal'] is None
                assert (files / 'Alpha report.txt').is_file()
                tree_click('deep')
                d.wait(lambda s: any(row.get('name') == 'leaf.txt' for row in s['fileTree']['rows']))
                capture('02-expanded-folders')
                tree_click('level-00')
                d.wait(lambda s: not s['fileTree']['loading']
                       and any(row.get('path') == str(deep_directories[0]) and row['expanded']
                               for row in s['fileTree']['rows']))
                for directory in deep_directories[1:]:
                    d.key('right')
                    revealed(directory)
                    d.key('right')
                    d.wait(lambda s: not s['fileTree']['loading']
                           and any(row.get('path') == str(directory) and row['expanded']
                                   for row in s['fileTree']['rows']))
                d.key('right')
                state = revealed(deep_leaf)
                assert next(row['depth'] for row in state['fileTree']['rows']
                            if row.get('path') == str(deep_leaf)) > 24, state
                capture('09-deep-tree')
                d.key('home')
                d.wait(lambda s: s['fileTree']['selectedIndex'] == 0
                       and s['fileTree']['visibleRange'][0] == 0)
                tree_click('file-tree-tc-folder')
                d.wait(lambda s: not any(row.get('name') == 'deep' for row in s['fileTree']['rows']))
            case('TREE-03', 'Unindexed/empty folders, 24-level nesting, keyboard navigation and independent focus', expand_and_keyboard)

            def follow_and_scroll():
                state = select('한글')
                assert state['fileTree']['selectedIndex'] > 320, state
                capture('03-auto-scroll')
                for text in ['Alpha', '한글', 'child.txt']:
                    select(text)
                state = revealed(files / 'folder-demo/child.txt')
                assert state['fileTree']['root'] == str(files), state
                assert any(row.get('name') == 'file-tree-tc-sibling.md' for row in state['fileTree']['rows']), state
                capture('04-tree-dark')
                d.send('appearance', theme='light')
                d.wait(lambda s: s['theme'] == 'light')
                capture('05-tree-light')
                d.send('appearance', theme='dark')
                d.wait(lambda s: s['theme'] == 'dark')
                select('Alpha', 'Alpha report.txt')
                d.key('shift-down')
                state = d.wait(lambda s: len(s['selected']) == 2)
                revealed(Path(state['rows'][state['primary']]['path']))
            case('TREE-04', 'Automatic ancestor expansion, long-directory scrolling, keyboard selection, dark/light themes', follow_and_scroll)

            def resize_and_toggle():
                state = d.state()
                edge = state['width'] - state['fileTree']['width']
                before = d.send('window')
                d.send('down', x=edge, y=15)
                d.send('move', x=edge - 60, y=15, drag=True)
                d.send('up', x=edge - 60, y=15)
                state = d.wait(lambda s: s['fileTree']['width'] >= 335)
                after = d.send('window')
                assert all(before[key] == after[key] for key in ['x', 'y', 'width', 'height'])
                capture('10-titlebar-resized')
                tree_width = state['fileTree']['width']
                state = toggle()
                assert state['columns'] == original_columns, (state['columns'], original_columns)
                state = toggle()
                assert state['fileTree']['width'] == tree_width, state
                d.send('resize', width=400, height=300)
                state = d.wait(lambda s: s['width'] == 400 and s['height'] == 300)
                assert state['fileTree']['width'] == 180, state
                assert state['width'] - state['fileTree']['width'] >= 220, state
                capture('06-small-window')
                d.send('resize', width=900, height=560)
                d.wait(lambda s: s['width'] == 900 and s['fileTree']['width'] == tree_width)
            case('TREE-05', 'Titlebar divider resizes the panel without moving the window, stable columns on toggle and usable 400×300 layout', resize_and_toggle)

            def refresh_and_file_actions():
                select('child.txt')
                refresh_file.write_text('Appeared after the directory was loaded\n')
                tree_header_click(21)
                d.wait(lambda s: any(row.get('name') == refresh_file.name for row in s['fileTree']['rows']))
                select('gpui-tc-rename.txt')
                d.click(90, 120)
                d.wait(lambda s: s['renamePath'] is not None)
                d.text('file-tree-tc-renamed')
                d.key('enter')
                revealed(files / 'file-tree-tc-renamed.txt')
                d.click(90, 120)
                d.wait(lambda s: s['renamePath'] is not None)
                d.text('gpui-tc-rename')
                d.key('enter')
                revealed(files / 'gpui-tc-rename.txt')
                select('gpui-tc-trash.txt')
                d.key('cmd-backspace')
                d.wait(lambda s: not s['actionBusy'] and s['selected'] == []
                       and not any(row.get('name') == 'gpui-tc-trash.txt' or row.get('message') == 'Loading…'
                                   for row in s['fileTree']['rows']))
                assert not (files / 'gpui-tc-trash.txt').exists()
            case('TREE-06', 'Refresh picks up disk changes; rename and Trash update the open tree', refresh_and_file_actions)

            def parent_and_persistence():
                select('child.txt')
                tree_header_click(47)
                d.wait(lambda s: s['fileTree']['root'] == str(fixture))
                revealed(files / 'folder-demo/child.txt')
                width = d.state()['fileTree']['width']
                restart()
                state = d.wait(lambda s: not s['busy'])
                assert state['fileTree']['visible'] and state['fileTree']['width'] == width, state
                select('child.txt')
                assert d.state()['fileTree']['root'] == str(files / 'folder-demo')
                select('keep.txt')
                assert d.state()['fileTree']['root'] == str(files / 'folder-demo-other')
                tree_header_click(47)
                revealed(files / 'folder-demo-other/keep.txt')
                capture('07-parent-navigation')
            case('TREE-07', 'Parent navigation, persisted visibility/width and following a different directory', parent_and_persistence)

            def unavailable_and_recovery():
                restart()
                d.wait_query('report_0000000.txt')
                d.click(90, 120)
                d.wait(lambda s: any(row.get('message') and row['message'] not in ['Loading…', 'Empty folder']
                                     for row in s['fileTree']['rows']))
                capture('08-unavailable-folder')
                select('Alpha', 'Alpha report.txt')
                state = revealed(files / 'Alpha report.txt')
                assert not any('message' in row for row in state['fileTree']['rows']), state
                d.wait_query('report_0000000.txt')
                d.click(90, 120)
                d.wait(lambda s: s['fileTree']['selectionError'] is not None)
                select('Alpha', 'Alpha report.txt')
                toggle()
                restart()
                assert not d.wait(lambda s: not s['busy'])['fileTree']['visible']
            case('TREE-08', 'Unavailable-folder message, recovery and persisted closed state', unavailable_and_recovery)

            def refresh_preserves_position():
                toggle()
                state = select('한글')
                before = state['fileTree']
                assert before['selectedIndex'] > 320, state
                position_file.write_text('Inserted above the visible tree rows\n')
                tree_header_click(21)
                d.wait(lambda s: not s['fileTree']['loading']
                       and s['fileTree']['rowCount'] == before['rowCount'] + 1)
                state = revealed(files / '한글 보고서.txt')
                assert abs(state['fileTree']['scrollOffset'] - (before['scrollOffset'] - 27)) < 1, state
                expected_offset = state['fileTree']['scrollOffset']
                d.send('hide')
                time.sleep(.15)
                d.send('activate')
                time.sleep(.3)
                state = revealed(files / '한글 보고서.txt')
                assert abs(state['fileTree']['scrollOffset'] - expected_offset) < 1, state
                assert state['selected'] == [str(files / '한글 보고서.txt')], state
                capture('11-refresh-position')
            case('TREE-09', 'Refresh and window reactivation preserve the selected file and viewport, including insertion above it', refresh_preserves_position)

            def persistent_scrollbars():
                d.wait_query('report')
                d.send('appearance', theme='dark')
                d.wait(lambda s: s['theme'] == 'dark')
                d.send('move', x=120, y=15)
                # Both thumbs must remain visible and draggable after the
                # default scrolling-mode fade timeout, without hovering them.
                time.sleep(3.2)
                capture('12-idle-scrollbars-dark')
                state = d.state()
                tree = state['fileTree']
                x, y, width, height = tree['listBounds']
                d.send('down', x=x + width - 8, y=y + height - 24)
                d.send('move', x=x + width - 8, y=y + height / 2, drag=True)
                d.send('up', x=x + width - 8, y=y + height / 2)
                state = d.wait(lambda s: s['fileTree']['scrollOffset'] > tree['scrollOffset'] + 100)
                assert state['fileTree']['selected'] == tree['selected'], state
                browsing_offset = state['fileTree']['scrollOffset']
                tree_header_click(21)
                time.sleep(.3)
                state = d.wait(lambda s: not s['fileTree']['loading'])
                assert abs(state['fileTree']['scrollOffset'] - browsing_offset) < 1, state

                # The results scrollbar is anchored below the column header.
                edge = state['width'] - state['fileTree']['width']
                d.send('down', x=edge - 8, y=state['resultsTop'] + 24)
                d.send('move', x=edge - 8, y=state['resultsTop'] + 124, drag=True)
                d.send('up', x=edge - 8, y=state['resultsTop'] + 124)
                state = d.wait(lambda s: s['visibleRange'][0] > 10)
                assert state['selected'] == [], state
                assert abs(state['fileTree']['scrollOffset'] - browsing_offset) < 1, state
                bar_y = state['height'] - 33 - 8
                d.send('down', x=edge / 2, y=bar_y)
                d.send('move', x=edge / 2 + 30, y=bar_y, drag=True)
                d.send('up', x=edge / 2 + 30, y=bar_y)
                d.wait(lambda s: s['horizontalOffset'] < -10)
                capture('13-independent-scrollbars')
                d.send('appearance', theme='light')
                d.wait(lambda s: s['theme'] == 'light')
                d.send('move', x=120, y=15)
                time.sleep(3.2)
                capture('14-idle-scrollbars-light')
            case('TREE-10', 'Idle scrollbars stay visible, both thumbs drag independently, and refreshing preserves manual browsing', persistent_scrollbars)

            def copy_displayed_paths():
                selected = files / '한글 보고서.txt'
                state = select('한글')
                d.click(30, state['height'] - 16)
                d.wait(lambda _: d.send('clipboard')['text'] == str(selected))
                assert d.state()['selected'] == [str(selected)]

                def copy_tree_path():
                    state = d.state()
                    tree = state['fileTree']
                    d.click(tree['listBounds'][0] + 20, tree['listBounds'][1] - 13)
                    assert d.send('clipboard')['text'] == tree['root']
                    after = d.state()
                    assert after['query'] == '한글' and after['selected'] == [str(selected)], after
                    assert after['fileTree']['root'] == tree['root'], after
                    assert after['fileTree']['scrollOffset'] == tree['scrollOffset'], after

                # The root is displayed as ~ but copies its absolute path.
                copy_tree_path()
                tree_header_click(47)
                d.wait(lambda s: s['fileTree']['root'] == str(fixture)
                       and not s['fileTree']['loading'])
                revealed(selected)
                d.send('resize', width=400, height=300)
                state = d.wait(lambda s: s['width'] == 400 and s['height'] == 300)
                # The tree path is truncated in the narrow window; the status
                # path still uses ~. Both must copy the full absolute path.
                d.click(30, state['height'] - 16)
                assert d.send('clipboard')['text'] == str(selected)
                copy_tree_path()
                capture('15-copy-path')
            case('TREE-11', 'Status and tree path clicks copy full absolute Unicode paths, including ~ and truncation, without changing selection or navigation', copy_displayed_paths)
        finally:
            if app:
                app.close()
            (files / 'file-tree-tc-renamed.txt').unlink(missing_ok=True)
            for path, content in originals.items():
                if content is None:
                    path.unlink(missing_ok=True)
                else:
                    path.write_bytes(content)
            for path in [empty, empty.parent / 'deep', *reversed(deep_directories), empty.parent]:
                path.rmdir()

    report = dict(createdAt=time.time(), binary=str(binary), fixture=str(fixture), results=results,
                  passed=sum(result['status'] == 'pass' for result in results),
                  failed=sum(result['status'] == 'fail' for result in results))
    (output / 'file-tree-results.json').write_text(json.dumps(report, ensure_ascii=False, indent=2))
    return report['failed'] == 0


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'app/target/release/everything')
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=ROOT / '.artifacts/file-tree-ui')
    args = parser.parse_args()
    raise SystemExit(0 if run(args.binary.resolve(), args.fixture, args.output.resolve()) else 1)
