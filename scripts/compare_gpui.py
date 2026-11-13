#!/usr/bin/env python3
"""Repeatable release Tauri/GPUI comparison on one marked, immutable fixture.

Engine parity compares complete returned path arrays, count and search mode.
UI latency includes each renderer's actual debounce and submitted-frame callbacks.
macOS RSS/physical footprint include newly created WebKit XPC processes, even
when launchd is their parent. Process association and exit evidence are retained.
"""
import argparse
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile
import time

from ui_driver import Driver

ROOT=Path(__file__).resolve().parents[1]


def percentile(values,p):
    ordered=sorted(values)
    return ordered[max(0,min(len(ordered)-1,math.ceil(len(ordered)*p)-1))]


def stats(values):
    return dict(n=len(values),p50=statistics.median(values),p95=percentile(values,.95),min=min(values),max=max(values))


def read_metrics(path):
    if not path.exists():return []
    rows=[]
    for line in path.read_text().splitlines():
        try:rows.append(json.loads(line))
        except ValueError:pass
    return rows


def processes():
    result={}
    for line in subprocess.check_output(['ps','-axo','pid=,ppid=,rss=,command='],text=True).splitlines():
        fields=line.strip().split(None,3)
        if len(fields)==4:
            result[int(fields[0])]=dict(pid=int(fields[0]),ppid=int(fields[1]),rssKiB=int(fields[2]),command=fields[3])
    return result


class Usage(ctypes.Structure):
    _fields_=[('uuid',ctypes.c_uint8*16)]+[(name,ctypes.c_uint64) for name in
        ['user','system','pkgIdleWakeups','interruptWakeups','pageins','wired','resident','footprint','start','exit',
         'childUser','childSystem','childPkgWakeups','childInterruptWakeups','childPageins','childElapsed','diskRead','diskWrite']]


LIB=ctypes.CDLL(None,use_errno=True)
LIB.proc_pid_rusage.argtypes=[ctypes.c_int,ctypes.c_int,ctypes.c_void_p]
LIB.proc_pid_rusage.restype=ctypes.c_int
RESP=LIB.responsibility_get_pid_responsible_for_pid
RESP.argtypes=[ctypes.c_int];RESP.restype=ctypes.c_int


class Timebase(ctypes.Structure):
    _fields_=[('numer',ctypes.c_uint32),('denom',ctypes.c_uint32)]


TIMEBASE=Timebase()
LIB.mach_timebase_info.argtypes=[ctypes.POINTER(Timebase)]
LIB.mach_timebase_info.restype=ctypes.c_int
if LIB.mach_timebase_info(ctypes.byref(TIMEBASE))!=0 or TIMEBASE.denom==0:
    raise RuntimeError('Cannot convert macOS CPU accounting ticks')


def usage(pid):
    data=Usage()
    if LIB.proc_pid_rusage(pid,2,ctypes.byref(data))!=0:
        return dict(error=ctypes.get_errno())
    ticks=data.user+data.system
    return dict(residentBytes=data.resident,physicalFootprintBytes=data.footprint,cpuTicks=ticks,
                cpuNs=ticks*TIMEBASE.numer//TIMEBASE.denom,responsiblePid=RESP(pid))


def environment():
    current=processes()
    daemons=[];builds=[]
    for process in current.values():
        command=process['command'].split()
        executable=Path(command[0]).name
        if executable in ['Everything','everything'] and '--daemon' in command:
            daemons.append(dict(**process,**usage(process['pid']),sampledAt=time.time()))
        if executable in ['cargo','rustc','clang','clang++','hdiutil','codesign']:
            builds.append(process)
    def output(*args):
        return subprocess.check_output(args,text=True).strip()
    return dict(date=time.strftime('%Y-%m-%dT%H:%M:%S%z'),cpu=output('sysctl','-n','machdep.cpu.brand_string'),
                memoryBytes=int(output('sysctl','-n','hw.memsize')),os=output('sw_vers'),rust=output('rustc','--version'),
                commonDaemon=daemons,loadAverage=output('sysctl','-n','vm.loadavg'),buildProcesses=builds,
                cpuTimebase=dict(numer=TIMEBASE.numer,denom=TIMEBASE.denom))


class PerfApp:
    def __init__(self,binary,fixture,work,label,page_size=None):
        self.baseline=processes()
        self.owned={}
        self.socket=work/f'{label}.sock';self.metrics=work/f'{label}.jsonl'
        self.socket.unlink(missing_ok=True);self.metrics.unlink(missing_ok=True)
        (fixture/'data/gpui-preferences.json').write_text(json.dumps(dict(theme='system',columns=[0,0,0,0],window=[100,100,900,560],fdaDismissed=False)))
        env=dict(os.environ)
        for key in ['EVERYTHING_UI_STATE','EVERYTHING_UI_TEST_ONBOARDING','EVERYTHING_UI_BENCH_PAGE_SIZE']:
            env.pop(key,None)
        env.update(EVERYTHING_UI_TEST='1',EVERYTHING_DATA_DIR=str(fixture/'data'),
                   EVERYTHING_UI_METRICS=str(self.metrics),EVERYTHING_UI_AUTOMATION_SOCKET=str(self.socket))
        if page_size:env['EVERYTHING_UI_BENCH_PAGE_SIZE']=str(page_size)
        self.log=(work/f'{label}.log').open('w')
        started=time.perf_counter()
        self.process=subprocess.Popen([str(binary)],env=env,stdout=self.log,stderr=subprocess.STDOUT)
        self.driver=Driver(self.socket)
        deadline=time.monotonic()+25
        while time.monotonic()<deadline:
            if self.process.poll() is not None:raise RuntimeError(f'{label} exited: {self.log.name}')
            rows=read_metrics(self.metrics)
            frame=next((m for m in rows if m['event']=='firstFrame'),None)
            if frame is not None:
                self.startup=dict(callbackMs=frame['elapsedMs'],wallToCallbackMs=(time.perf_counter()-started)*1000)
                break
            time.sleep(.005)
        else:
            self.close();raise TimeoutError(f'No submitted frame: {label}')
        self.driver.send('resize',width=900,height=560)

    def sample(self):
        current=processes()
        selected={self.process.pid}
        while True:
            additions={pid for pid,p in current.items() if p['ppid'] in selected}
            if additions<=selected:break
            selected|=additions
        for pid,p in current.items():
            webkit='WebKit' in p['command'] and any(s in p['command'] for s in ['WebContent','GPU','Networking'])
            if webkit and pid not in self.baseline:
                selected.add(pid)
                self.owned.setdefault(pid,dict(**p,association='new WebKit XPC during isolated app run'))
        rows=[]
        for pid in sorted(selected):
            if pid not in current:continue
            p=current[pid];u=usage(pid)
            rows.append(dict(**p,**u))
            self.owned.setdefault(pid,dict(**p,association='main or descendant'))
        return dict(at=time.time(),processes=rows,rssMiB=sum(p['rssKiB'] for p in rows)/1024,
                    footprintMiB=sum(p.get('physicalFootprintBytes',0) for p in rows)/1048576,
                    cpuNs=sum(p.get('cpuNs',0) for p in rows))

    def query(self,text):
        before=len(read_metrics(self.metrics))
        started=time.perf_counter()
        self.driver.query(text)
        deadline=time.monotonic()+20
        while time.monotonic()<deadline:
            rows=read_metrics(self.metrics)[before:]
            match=next((r for r in rows if r['event']=='searchFrame' and r['query']==text),None)
            if match:return dict(**match,wallToCallbackMs=(time.perf_counter()-started)*1000)
            time.sleep(.004)
        raise TimeoutError(f'No result frame for {text!r}: {self.log.name}')

    def close(self):
        if self.process.poll() is None:
            try:self.driver.key('cmd-q')
            except (OSError,ValueError):pass
            try:self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.terminate();self.process.wait(timeout=5)
        time.sleep(.3)
        remaining=processes()
        owned=[dict(**p,aliveAfterAppExit=pid in remaining) for pid,p in sorted(self.owned.items())]
        self.log.close();self.socket.unlink(missing_ok=True)
        return owned


def suite(fixture):
    files=fixture/'files'
    queries=[]
    def add(identifier,query,**values):queries.append(dict(id=identifier,query=query,**values))
    for identifier,text in [('empty',''),('name','report'),('case','REPORT'),('alpha','Alpha'),('korean','한글'),('spaces','space name'),('multi-term','report final'),('unicode','unicode-École'),('extension','*.md'),('extension-case','*.TXT'),('glob-question','report_000000?.txt'),('glob-contains','*final*'),('glob-mixed','report*pdf'),('no-result','no-such-gpui-file-xyz'),('short','r')]:add(identifier,text)
    for identifier,text in [('absolute-directory',str(files/'folder-demo')+'/'),('absolute-file',str(files/'space name.txt')),('relative-directory','folder-demo/'),('relative-file','folder-demo/child'),('sibling-directory','folder-demo-other/'),('home-directory','~/folder-demo/'),('directory-glob',str(files/'folder-demo')+'/*.txt')]:add(identifier,text)
    for key in ['name','dir','size','mtime']:
        for direction in ['asc','desc']:
            add(f'sort-{key}-{direction}','report',sortBy=key,sortDir=direction)
    for offset in [0,300,600,999]:add(f'page-{offset}','report',offset=offset,limit=300)
    add('short-next','r',offset=100,limit=300)
    add('max-limit','report',limit=1000)
    return dict(warmups=3,iterations=10,queries=queries)


def engine_compare(a,b,fixture,output):
    specification=suite(fixture)
    path=output/'search-suite.json';path.write_text(json.dumps(specification,ensure_ascii=False,indent=2)+'\n')
    env=dict(os.environ,EVERYTHING_DATA_DIR=str(fixture/'data'))
    reports=[]
    for label,binary in [('tauri',a),('gpui',b)]:
        started=time.monotonic()
        completed=subprocess.run([str(binary),'--search-bench',str(path.resolve())],env=env,capture_output=True,text=True,check=True)
        report=json.loads(completed.stdout)
        (output/f'{label}-engine.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n')
        reports.append(report)
        print(f'{label}: {len(report["cases"])} engine cases in {time.monotonic()-started:.1f}s',flush=True)
    comparisons=[]
    for old,new in zip(reports[0]['cases'],reports[1]['cases']):
        equal=all(old[k]==new[k] for k in ['id','paths','mode','total'])
        comparisons.append(dict(id=old['id'],equal=equal,count=len(new['paths']),total=new['total'],mode=new['mode'],
                                pathsSha256=hashlib.sha256(json.dumps(new['paths']).encode()).hexdigest(),tauriP50Ms=old['p50Ms'],gpuiP50Ms=new['p50Ms']))
    assert all(c['equal'] for c in comparisons),[c for c in comparisons if not c['equal']]
    expected={'alpha':2,'korean':1,'spaces':1,'extension':62502,'no-result':0,'relative-directory':1,'absolute-directory':1}
    for identifier,count in expected.items():
        c=next(c for c in comparisons if c['id']==identifier)
        assert c['total']==count,c
    return dict(cases=len(comparisons),allEqual=True,comparisons=comparisons,
                tauriMs=stats([n for c in reports[0]['cases'] for n in c['samplesMs']]),
                gpuiMs=stats([n for c in reports[1]['cases'] for n in c['samplesMs']]))


def ui_compare(a,b,fixture,work,output,trials,iterations):
    binaries={'tauri':a,'gpui':b}
    starts={key:[] for key in binaries}
    for trial in range(trials):
        for label in (['tauri','gpui'] if trial%2==0 else ['gpui','tauri']):
            app=PerfApp(binaries[label],fixture,work,f'start-{label}-{trial}')
            starts[label].append(app.startup)
            app.close()
        print(f'Startup trial {trial+1}/{trials}',flush=True)
    results={}
    texts=['report','한글','Alpha','*.md','space name','no-such-gpui-file-xyz']
    for label in ['tauri','gpui']:
        app=PerfApp(binaries[label],fixture,work,f'bench-{label}')
        measurements=[];memory={}
        try:
            time.sleep(1.5)
            memory['empty']=[app.sample() for _ in range(3)]
            for _ in range(2):
                for text in texts:app.query(text)
            for iteration in range(iterations):
                for text in texts:measurements.append(dict(iteration=iteration,**app.query(text)))
            app.query('report');time.sleep(1)
            memory['results']=[app.sample() for _ in range(5)]
            s0=app.sample();time.sleep(2);s1=app.sample()
            idle_cpu=(s1['cpuNs']-s0['cpuNs'])/1e9/(s1['at']-s0['at'])*100
            # Churn is separate from latency collection. State-file writes stay disabled.
            for iteration in range(20):
                for text in texts:app.query(text)
            app.query('report');time.sleep(1)
            memory['after120Queries']=[app.sample() for _ in range(5)]
            app.driver.send('capture',path=str((output/f'{label}-benchmark.png').resolve()))
        finally:
            owned=app.close()
        results[label]=dict(startups=starts[label],startupWallMs=stats([s['wallToCallbackMs'] for s in starts[label]]),
            startupCallbackMs=stats([s['callbackMs'] for s in starts[label]]),searchFrames=measurements,
            inputToFrameMs=stats([m['elapsedMs'] for m in measurements]),backendCallMs=stats([m['backendMs'] for m in measurements]),
            wallInputToCallbackMs=stats([m['wallToCallbackMs'] for m in measurements]),idleCpuPercent=idle_cpu,memory=memory,associatedProcesses=owned)
        print(f'{label}: p50={results[label]["inputToFrameMs"]["p50"]:.1f}ms, RSS={statistics.median(s["rssMiB"] for s in memory["results"]):.1f}MiB',flush=True)
    return results


if __name__=='__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--tauri',type=Path,required=True);p.add_argument('--gpui',type=Path,default=ROOT/'src-tauri/target/release/everything')
    p.add_argument('--fixture',type=Path,required=True);p.add_argument('--output',type=Path,default=ROOT/'doc/gpui-evidence/performance')
    p.add_argument('--startup-trials',type=int,default=7);p.add_argument('--iterations',type=int,default=10)
    p.add_argument('--engine-only',action='store_true');p.add_argument('--ui-only',action='store_true')
    args=p.parse_args();fixture=args.fixture.resolve();args.output.mkdir(parents=True,exist_ok=True)
    if not (fixture/'.everything-ui-fixture').exists():raise SystemExit('Use a marked isolated fixture')
    env_report=environment()
    if env_report['buildProcesses']:raise SystemExit('Build processes are active; run the comparison after they finish')
    (args.output/'environment.json').write_text(json.dumps(env_report,ensure_ascii=False,indent=2)+'\n')
    result=dict(date=time.strftime('%Y-%m-%dT%H:%M:%S%z'),platform=platform.platform(),machine=platform.machine(),
                fixture=json.loads((fixture/'fixture.json').read_text()),binaries=dict(tauri=str(args.tauri.resolve()),gpui=str(args.gpui.resolve())),
                binarySha256={label:hashlib.sha256(binary.read_bytes()).hexdigest() for label,binary in [('tauri',args.tauri),('gpui',args.gpui)]})
    if not args.ui_only:result['engine']=engine_compare(args.tauri.resolve(),args.gpui.resolve(),fixture,args.output)
    if not args.engine_only:
        with tempfile.TemporaryDirectory(prefix='ev-perf-') as temporary:
            result['ui']=ui_compare(args.tauri.resolve(),args.gpui.resolve(),fixture,Path(temporary),args.output,args.startup_trials,args.iterations)
    (args.output/'comparison.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
    print('Comparison saved:',args.output/'comparison.json',flush=True)
