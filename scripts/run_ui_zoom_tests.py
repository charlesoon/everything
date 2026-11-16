#!/usr/bin/env python3
"""Check browser-style zoom through real native input in a marked fixture."""
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
    prefs = fixture / 'data/gpui-preferences.json'
    prefs.write_text(json.dumps(dict(window=[100, 100, 1100, 700],
                                    fileTreeVisible=True, fileTreeWidth=280)))
    extras = [fixture / 'files' / f'ui-zoom-tc-scroll-{i:03}.txt' for i in range(140)]
    originals = {path: path.read_bytes() if path.exists() else None for path in extras}
    for path in extras:
        path.write_text('Isolated zoom fixture\n')
    results = []
    app = None
    with tempfile.TemporaryDirectory(prefix='ev-ui-zoom-') as temporary:
        work = Path(temporary)
        try:
            app = App(binary, fixture, work)
            d = app.driver
            d.send('appearance', theme='dark')
            d.wait(lambda s: not s['busy'] and s['resultsTop'] > 0)

            def capture(name):
                time.sleep(.1)
                d.send('capture', path=str(output / f'{name}.png'))

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
                    capture(f'{identifier}-failure')
                    print(f'FAIL {identifier}: {error}', flush=True)
                    d.key('escape')
                    d.key('cmd-0')

            def settled_zoom(expected):
                return d.wait(lambda s: abs(s['zoom'] - expected) < .005
                              and (s['modal'] is not None
                                   or abs(s['resultsTop'] - (38 + 77 * expected)) < 1)
                              and abs(s['rowHeight'] - 27 * expected) < .1)

            def zoom(key, expected):
                d.key(key)
                return settled_zoom(expected)

            def toggle_zoom():
                state = d.state()
                assert state['zoomPopover']['buttonVisible'], state
                x, y, width, height = state['zoomPopover']['buttonBounds']
                d.click(x + width / 2, y + height / 2)
                return d.wait(lambda s: s['zoomPopover']['visible']
                              != state['zoomPopover']['visible'])

            def click_control(index, expected):
                state = d.state()
                assert state['zoomPopover']['visible'], state
                x, y, width, height = state['zoomPopover']['bounds']
                d.click(x + width - [135, 99, 45][index], y + height / 2)
                return settled_zoom(expected)

            def select(query, index=0):
                state = d.wait_query(query)
                d.click(80, state['resultsTop'] + (index + .5) * state['rowHeight'])
                return d.wait(lambda s: len(s['selected']) == 1
                              and not s['fileTree']['loading'])

            def shortcuts():
                state = d.wait_query('Alpha')
                assert not state['zoomPopover']['buttonVisible'], state
                before = d.send('window')
                for key, scale in [('cmd-equals', 1.1), ('cmd-shift-plus', 1.25),
                                   ('ctrl-minus', 1.1), ('cmd-0', 1.),
                                   ('ctrl-plus', 1.1), ('cmd-minus', 1.)]:
                    state = zoom(key, scale)
                    assert state['query'] == 'Alpha' and state['selected'] == [], state
                    assert state['zoomPopover']['visible'] and state['zoomPopover']['buttonVisible'], state
                assert d.send('window') == before
                d.key('plus')
                d.wait(lambda s: s['query'] == 'Alpha+' and s['zoom'] == 1.)
                d.key('minus')
                state = d.wait(lambda s: s['query'] == 'Alpha+-' and s['zoom'] == 1.)
                assert not state['zoomPopover']['buttonVisible'], state
                capture('01-zoom-100')
            case('ZOOM-01', 'Command/Control, =/Shift+plus, minus and reset work in the search input without inserting shortcut characters or resizing the window', shortcuts)

            def scaled_selection_and_copy():
                state = select('한글')
                selected = state['selected']
                root = state['fileTree']['root']
                for key, scale in [('cmd-plus', 1.1), ('cmd-plus', 1.25), ('cmd-plus', 1.5)]:
                    state = zoom(key, scale)
                assert state['query'] == '한글' and state['selected'] == selected, state
                assert state['fileTree']['root'] == root, state
                d.click(30, state['height'] - 16 * state['zoom'])
                assert d.send('clipboard')['text'] == selected[0]
                tree = state['fileTree']
                d.click(tree['listBounds'][0] + 20, tree['listBounds'][1] - 13 * state['zoom'])
                assert d.send('clipboard')['text'] == root
                capture('02-zoom-150')
                state = select('Alpha', 1)
                assert state['selected'][0].endswith('alpha-summary.TXT'), state
            case('ZOOM-02', '150% layout keeps selection and navigation, and scaled result/path hit targets select and copy the correct file', scaled_selection_and_copy)

            def rename():
                zoom('cmd-0', 1.)
                state = select('gpui-tc-rename.txt')
                d.click(80, state['resultsTop'] + state['rowHeight'] / 2)
                before = d.wait(lambda s: s['renamePath'] is not None)
                state = zoom('cmd-plus', 1.1)
                assert state['renamePath'] == before['renamePath'], state
                assert state['renameValue'] == before['renameValue'], state
                zoom('cmd-0', 1.)
                d.key('escape')
                d.wait(lambda s: not s['zoomPopover']['visible'] and s['renamePath'] is not None)
                d.key('escape')
                d.wait(lambda s: s['renamePath'] is None)
            case('ZOOM-03', 'Zoom works during inline rename without committing, cancelling or editing the filename', rename)

            def tree_scroll_and_resize():
                state = select('Alpha')
                tree = state['fileTree']
                row = next(row for row in tree['rows'] if row.get('path') == state['selected'][0])
                d.click(tree['listBounds'][0] + 70,
                        tree['listBounds'][1] + row['index'] * state['rowHeight']
                        + tree['scrollOffset'] + state['rowHeight'] / 2)
                before = d.wait(lambda s: s['fileTree']['focused'])
                for _ in range(2):
                    previous_index = before['fileTree']['selectedIndex']
                    d.key('pagedown')
                    before = d.wait(lambda s: s['fileTree']['selectedIndex'] > previous_index
                                    and s['fileTree']['visibleRange'][0]
                                    <= s['fileTree']['selectedIndex']
                                    < s['fileTree']['visibleRange'][1])
                assert before['fileTree']['scrollOffset'] < -100, before
                state = zoom('cmd-plus', 1.1)
                assert state['fileTree']['focused'], state
                assert state['fileTree']['selected'] == before['fileTree']['selected'], state
                assert abs(state['fileTree']['scrollOffset'] - before['fileTree']['scrollOffset'] * 1.1) < 2, state
                state = zoom('cmd-plus', 1.25)
                width = state['fileTree']['width']
                edge = state['width'] - width
                d.send('down', x=edge, y=18)
                d.send('move', x=edge - 25, y=18, drag=True)
                d.send('up', x=edge - 25, y=18)
                state = d.wait(lambda s: abs(s['fileTree']['width'] - width - 25) < 1)
                assert state['query'] == 'Alpha', state
                d.send('resize', width=1400, height=700)
                state = d.wait(lambda s: s['width'] == 1400 and s['columns'][0] > 250)
                columns = state['columns']
                header_y = state['resultsTop'] - 16.5 * state['zoom']
                d.send('down', x=columns[0], y=header_y)
                d.send('move', x=columns[0] + 25, y=header_y, drag=True)
                d.send('up', x=columns[0] + 25, y=header_y)
                state = d.wait(lambda s: abs(s['columns'][0] - columns[0] - 25) < 1)
                assert abs(state['columns'][1] - columns[1] + 25) < 1, state
                capture('03-zoom-125-tree')
            case('ZOOM-04', 'Tree-focused zoom preserves the viewed rows and selection; titlebar resizing uses the scaled sidebar width', tree_scroll_and_resize)

            def limits_and_small_window():
                for _ in range(20):
                    d.key('cmd-plus')
                d.wait(lambda s: s['zoom'] == 2.)
                d.send('resize', width=400, height=300)
                state = d.wait(lambda s: s['width'] == 400 and s['height'] == 300
                               and abs(s['resultsTop'] - 192) < 1)
                assert state['fileTree']['width'] < state['width'], state
                capture('04-zoom-200-small')
                d.click(state['width'] - 29 * state['zoom'],
                        state['height'] - 16 * state['zoom'])
                d.wait(lambda s: s['modal'] == 'settings')
                capture('04b-zoom-200-small-settings')
                d.key('escape')
                for _ in range(30):
                    d.key('cmd-minus')
                state = d.wait(lambda s: s['zoom'] == .5 and abs(s['resultsTop'] - 76.5) < 1)
                assert state['rowHeight'] == 13.5, state
                capture('05-zoom-50-small')
                zoom('cmd-0', 1.)
                d.send('resize', width=1100, height=700)
                d.wait(lambda s: s['width'] == 1100 and s['height'] == 700)
            case('ZOOM-05', 'Repeated shortcuts stop at 50% and 200%; the 400x300 window stays usable and reset restores the layout', limits_and_small_window)

            def persistence_and_settings():
                nonlocal app, d
                zoom('cmd-plus', 1.1)
                before = zoom('cmd-plus', 1.25)
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if abs(json.loads(prefs.read_text()).get('zoom', 0) - 1.25) < .005:
                        break
                    time.sleep(.025)
                else:
                    raise AssertionError('Zoom was not saved')
                app.close()
                app = App(binary, fixture, work)
                d = app.driver
                state = d.wait(lambda s: s['zoom'] == 1.25 and not s['busy'])
                assert not state['zoomPopover']['visible'] and state['zoomPopover']['buttonVisible'], state
                assert state['width'] == before['width'] and state['height'] == before['height'], state
                assert state['columns'] == before['columns'], state
                d.send('appearance', theme='light')
                d.wait(lambda s: s['theme'] == 'light' and s['zoom'] == 1.25)
                capture('06-zoom-125-restart-light')
                state = zoom('cmd-0', 1.)
                d.click(state['width'] - 29, state['height'] - 16)
                d.wait(lambda s: s['modal'] == 'settings')
                state = zoom('cmd-plus', 1.1)
                assert state['modal'] == 'settings', state
                state = click_control(1, 1.25)
                assert state['modal'] == 'settings', state
                capture('07-zoom-settings')
                d.key('escape')
                d.wait(lambda s: not s['zoomPopover']['visible'] and s['modal'] == 'settings')
                d.key('escape')
                d.wait(lambda s: s['modal'] is None)
                zoom('cmd-0', 1.)
                d.key('escape')
                d.wait(lambda s: not s['zoomPopover']['visible'])
            case('ZOOM-06', 'Zoom and sidebar/column proportions survive restart and theme changes; shortcuts also work with the settings popover open', persistence_and_settings)

            def mouse_controls():
                d.send('appearance', theme='dark')
                before = select('Alpha')
                window = d.send('window')
                zoom('cmd-plus', 1.1)
                d.key('escape')
                state = toggle_zoom()
                assert state['zoomPopover']['pinned'], state
                click_control(1, 1.25)
                state = click_control(1, 1.5)
                assert state['zoomPopover']['buttonBounds'][2] == 26, state
                capture('08-zoom-popover-dark')
                click_control(0, 1.25)
                click_control(2, 1.)
                state = click_control(2, 1.)  # Disabled reset at the default scale.
                assert state['zoomPopover']['visible'] and state['zoomPopover']['buttonVisible'], state
                capture('08b-zoom-reset-100')
                assert state['query'] == before['query'], state
                assert state['selected'] == before['selected'], state
                assert state['fileTree']['root'] == before['fileTree']['root'], state
                assert d.send('window') == window
                state = toggle_zoom()  # The retained 100% icon can close both.
                assert not state['zoomPopover']['buttonVisible'], state
                capture('08c-zoom-reset-100-closed')
                state = zoom('cmd-0', 1.)
                assert state['zoomPopover']['visible'] and state['zoomPopover']['buttonVisible'], state
                d.click(160, 60)  # Outside closes it and still focuses search.
                state = d.wait(lambda s: not s['zoomPopover']['visible'])
                assert state['query'] == 'Alpha' and not state['tableFocused'] and not state['zoomPopover']['buttonVisible'], state
                zoom('cmd-plus', 1.1)
                d.key('escape')
                toggle_zoom()
                toggle_zoom()
                zoom('cmd-0', 1.)
                d.key('escape')
            case('ZOOM-07', 'Titlebar toggle and real mouse minus/plus/reset controls preserve input, selection and window bounds; outside clicks continue to the search input', mouse_controls)

            def automatic_visibility():
                d.send('move', x=30, y=200)
                state = zoom('cmd-plus', 1.1)
                assert state['zoomPopover']['visible'] and not state['zoomPopover']['pinned'], state
                time.sleep(1.7)
                zoom('cmd-plus', 1.25)
                time.sleep(1.7)
                assert d.state()['zoomPopover']['visible']
                d.wait(lambda s: not s['zoomPopover']['visible'], timeout=5)
                state = zoom('cmd-plus', 1.5)
                x, y, width, height = state['zoomPopover']['bounds']
                d.send('move', x=x + 20, y=y + height / 2)
                d.wait(lambda s: s['zoomPopover']['hovered'])
                time.sleep(3.2)
                assert d.state()['zoomPopover']['visible']
                click_control(0, 1.25)
                d.send('move', x=30, y=200)
                d.wait(lambda s: not s['zoomPopover']['visible'], timeout=5)
                state = zoom('cmd-0', 1.)
                assert state['zoomPopover']['visible'] and state['zoomPopover']['buttonVisible'], state
                state = d.wait(lambda s: not s['zoomPopover']['visible'], timeout=5)
                assert not state['zoomPopover']['buttonVisible'], state
                zoom('cmd-0', 1.)
                d.send('hide')
                time.sleep(.1)
                d.send('activate')
                state = d.wait(lambda s: not s['zoomPopover']['visible'])
                assert not state['zoomPopover']['buttonVisible'], state
            case('ZOOM-08', 'Shortcut popovers reset the three-second timer on repeated zoom, remain open while hovered and close after leaving', automatic_visibility)

            def keyboard_controls():
                zoom('cmd-plus', 1.1)
                d.key('escape')
                state = toggle_zoom()
                query = state['query']
                d.key('tab')
                d.wait(lambda s: s['zoomPopover']['keyboardSelection'] == 0)
                d.key('right')
                d.wait(lambda s: s['zoomPopover']['keyboardSelection'] == 1)
                d.key('enter')
                settled_zoom(1.25)
                d.key('tab')
                d.wait(lambda s: s['zoomPopover']['keyboardSelection'] == 2)
                d.key('enter')
                state = settled_zoom(1.)
                assert state['zoomPopover']['visible'] and state['zoomPopover']['buttonVisible'], state
                d.key('shift-tab')
                d.wait(lambda s: s['zoomPopover']['keyboardSelection'] == 1)
                d.key('space')
                state = settled_zoom(1.1)
                assert state['query'] == query and state['zoomPopover']['visible'], state
                d.key('escape')
                d.wait(lambda s: not s['zoomPopover']['visible'])
                zoom('cmd-0', 1.)
                d.key('escape')
            case('ZOOM-09', 'Manual popovers support Tab/Shift+Tab, arrow navigation, Enter/Space and Escape without editing the search text', keyboard_controls)

            def popover_themes_and_small_window():
                zoom('cmd-plus', 1.1)
                d.key('escape')
                toggle_zoom()
                click_control(1, 1.25)
                state = click_control(1, 1.5)
                assert state['zoomPopover']['bounds'][2:] == [260, 52], state
                d.send('appearance', theme='light')
                d.wait(lambda s: s['theme'] == 'light')
                capture('09-zoom-popover-light')
                d.send('resize', width=400, height=300)
                d.wait(lambda s: s['width'] == 400 and s['height'] == 300)
                click_control(1, 1.75)
                click_control(1, 2.)
                state = click_control(1, 2.)  # Disabled zoom in at the maximum.
                x, y, width, height = state['zoomPopover']['bounds']
                assert width == 260 and height == 52 and x >= 8, state
                assert x + width <= state['width'] - 8, state
                capture('10-zoom-popover-small-200-light')
                d.send('appearance', theme='dark')
                d.wait(lambda s: s['theme'] == 'dark')
                capture('11-zoom-popover-small-200-dark')
                tree_visible = state['fileTree']['visible']
                button_bounds = state['zoomPopover']['buttonBounds']
                d.click(state['width'] - 29, 19)
                state = d.wait(lambda s: s['fileTree']['visible'] != tree_visible
                               and not s['zoomPopover']['visible'])
                assert state['zoomPopover']['buttonBounds'] == button_bounds, state
                toggle_zoom()
                click_control(2, 1.)
                for expected in [.9, .8, .75, .67, .5]:
                    click_control(0, expected)
                state = click_control(0, .5)  # Disabled zoom out at the minimum.
                assert state['zoomPopover']['bounds'][2:] == [260, 52], state
                capture('12-zoom-popover-small-50')
                click_control(2, 1.)
                d.key('escape')
                d.send('resize', width=1100, height=700)
                d.wait(lambda s: s['width'] == 1100 and s['height'] == 700)
            case('ZOOM-10', 'Light/dark popovers keep a fixed size at 50–200% in a 400x300 window; boundary buttons disable and the adjacent file-tree toggle still works', popover_themes_and_small_window)
        finally:
            if app:
                app.close()
            for path, original in originals.items():
                if original is None:
                    path.unlink(missing_ok=True)
                else:
                    path.write_bytes(original)
    report = dict(binary=str(binary), fixture=str(fixture), results=results,
                  passed=sum(case['status'] == 'pass' for case in results),
                  failed=sum(case['status'] == 'fail' for case in results))
    (output / 'zoom-results.json').write_text(json.dumps(report, indent=2, ensure_ascii=False) + '\n')
    return not report['failed']


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'app/target/release/everything')
    parser.add_argument('--fixture', type=Path, required=True)
    parser.add_argument('--output', type=Path, default=ROOT / '.artifacts/ui-zoom')
    args = parser.parse_args()
    raise SystemExit(0 if run(args.binary.resolve(), args.fixture, args.output.resolve()) else 1)
