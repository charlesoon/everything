#!/usr/bin/env python3
"""Capture the original Tauri renderer and GPUI with identical native events."""
import argparse
import json
from pathlib import Path
import tempfile
import time

from compare_gpui import PerfApp

ROOT=Path(__file__).resolve().parents[1]


def capture(tauri,gpui,fixture,output):
    output.mkdir(parents=True,exist_ok=True)
    records=[]
    with tempfile.TemporaryDirectory(prefix='ev-parity-') as tmp:
        for label,binary in [('tauri',tauri),('gpui',gpui)]:
            app=PerfApp(binary,fixture,Path(tmp),label)
            d=app.driver
            try:
                for theme in ['dark','light']:
                    d.send('appearance',theme=theme)
                    time.sleep(.3)
                    def shot(name):
                        time.sleep(.15)
                        path=(output/f'{label}-{theme}-{name}.png').resolve()
                        d.send('capture',path=str(path))
                        records.append(dict(renderer=label,theme=theme,state=name,file=path.name))
                    d.query('');time.sleep(1.2);shot('empty')
                    app.query('report');time.sleep(1.2);shot('results')
                    d.key('down');time.sleep(.15);shot('selected')
                    if label=='gpui':
                        d.click(90,120)
                        time.sleep(d.send('window')['doubleClickIntervalMs']/1000+.1)
                    else:
                        d.key('f2');time.sleep(.15)
                    shot('rename')
                    d.key('escape');d.key('delete');time.sleep(.15);shot('trash')
                    d.key('escape')
                    app.query('no-such-gpui-file-xyz');time.sleep(.3);shot('no-results')
            finally:
                app.close()
    (output/'captures.json').write_text(json.dumps(records,indent=2)+'\n')
    try:
        from PIL import Image,ImageChops,ImageStat
    except ImportError:
        return
    comparisons=[]
    for theme in ['dark','light']:
        for state in ['empty','results','selected','rename','trash','no-results']:
            a=Image.open(output/f'tauri-{theme}-{state}.png').convert('RGB')
            b=Image.open(output/f'gpui-{theme}-{state}.png').convert('RGB')
            if a.size!=b.size:raise AssertionError((a.size,b.size))
            diff=ImageChops.difference(a,b)
            comparisons.append(dict(theme=theme,state=state,width=a.width,height=a.height,meanAbsoluteChannelError=sum(ImageStat.Stat(diff).mean)/3))
            if state=='results':
                pair=Image.new('RGB',(1800,560));pair.paste(a.resize((900,560)),(0,0));pair.paste(b.resize((900,560)),(900,0))
                pair.save(output/f'comparison-{theme}.png')
    (output/'pixel-comparison.json').write_text(json.dumps(comparisons,indent=2)+'\n')


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--tauri',type=Path,required=True)
    p.add_argument('--gpui',type=Path,default=ROOT/'src-tauri/target/release/everything')
    p.add_argument('--fixture',type=Path,required=True)
    p.add_argument('--output',type=Path,default=ROOT/'doc/gpui-evidence/visual')
    a=p.parse_args()
    fixture=a.fixture.resolve()
    if not (fixture/'.everything-ui-fixture').is_file():raise SystemExit('Use a marked fixture')
    capture(a.tauri.resolve(),a.gpui.resolve(),fixture,a.output)
