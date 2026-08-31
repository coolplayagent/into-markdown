from pathlib import Path
import argparse,collections,hashlib,json,os,re,subprocess,sys,time
import psutil

ROOT=Path(__file__).resolve().parents[1]
OLD=ROOT.parent/'2026-08-29'
PYTHON=Path(os.environ.get('BENCHMARK_PYTHON',str(OLD/'tools/markitdown-venv/Scripts/python.exe')))
ANYDOC=Path(os.environ.get('BENCHMARK_ANYDOC_MODULE',str(OLD/'tools/anydoc/node_modules/@firecrawl/anydoc/anydoc.js')))
MAX_RSS=12*1024**3

def write(path,value):
    path.parent.mkdir(parents=True,exist_ok=True)
    temporary=path.with_suffix(path.suffix+'.tmp')
    temporary.write_text(json.dumps(value,ensure_ascii=False,indent=2),encoding='utf-8')
    for attempt in range(20):
        try:temporary.replace(path);return
        except PermissionError:
            if attempt==19:raise
            time.sleep(.05)

def environment(tool):
    env={k:v for k,v in os.environ.items() if not k.startswith(('INTO_MD_','AZURE_','OPENAI_','FIRECRAWL_'))}
    state=ROOT/'state'/tool
    for name in ['USERPROFILE','APPDATA','LOCALAPPDATA','TMP','TEMP']:
        value=state/name;value.mkdir(parents=True,exist_ok=True);env[name]=str(value)
    env.update(PYTHONIOENCODING='utf-8',PYTHONUTF8='1',NO_PROXY='*',no_proxy='*')
    return env

def monitor(command,env,stem,seconds):
    stem.parent.mkdir(exist_ok=True,parents=True)
    start=time.perf_counter();peak=0;cpu=0;reason=None
    with stem.with_suffix('.stdout.log').open('wb') as out,stem.with_suffix('.stderr.log').open('wb') as err:
        child=subprocess.Popen(command,env=env,cwd=ROOT,stdout=out,stderr=err)
        parent=psutil.Process(child.pid)
        while child.poll() is None:
            rss=0
            try:members=[parent,*parent.children(recursive=True)]
            except psutil.Error:members=[]
            for proc in members:
                try:rss+=proc.memory_info().rss
                except psutil.Error:pass
            peak=max(peak,rss)
            if rss>MAX_RSS:reason='memoryLimit'
            elif time.perf_counter()-start>seconds:reason='timeout'
            if reason:
                for proc in reversed(members):
                    try:proc.kill()
                    except psutil.Error:pass
                break
            time.sleep(.02)
        code=child.wait(timeout=15)
    return {'command':command,'exitCode':code,'wallMs':(time.perf_counter()-start)*1000,'peakRssBytes':peak,'termination':reason,'timeoutSeconds':seconds,'memoryLimitBytes':MAX_RSS}

def chunks(items):
    current=[];ext=None
    for item in items:
        if current and (item['extension']!=ext or len(current)==16 or item['bytes']>5*1024**2):
            yield current;current=[]
        ext=item['extension'];current.append(item)
        if item['bytes']>5*1024**2:yield current;current=[]
    if current:yield current

def native_results(report,items,out):
    if not report.exists():return []
    try:data=json.loads(report.read_text(encoding='utf-8-sig'))
    except (ValueError,OSError):return []
    by_id={i['id']:i for i in items};result=[]
    for entry in data.get('items',[]):
        raw=str(entry.get('input') or entry.get('source') or entry.get('inputPath') or '')
        match=re.search(r'([0-9a-f]{16})\.[^.\\/]+$',raw)
        if not match or match[1] not in by_id:continue
        ident=match[1];dest=out/f'{ident}.md';status=str(entry.get('status') or '').lower()
        size=dest.stat().st_size if dest.exists() else 0
        accepted=status in ['converted','success','succeeded','completed','written']
        row={'id':ident,'status':'success' if accepted and size else ('empty' if accepted else 'failed'),'output':str(dest) if dest.exists() else None,'outputBytes':size,'rawReport':entry,'processingDurationMs':entry.get('processingDurationMs'),'durationMs':entry.get('durationMs'),'errorCode':entry.get('reasonCode') or entry.get('errorCode'),'error':entry.get('message') or entry.get('error')}
        result.append(row)
    return result

def execute(tool,items,seq,single=False):
    runroot=ROOT/'runs'/tool/seq;runroot.mkdir(exist_ok=True,parents=True)
    out=runroot/'outputs';out.mkdir(exist_ok=True)
    group=runroot/'manifest.json';report=runroot/'report.json';write(group,{'items':items})
    env=environment(tool)
    if tool.startswith('into-'):
        _,version,mode=tool.split('-');binary=ROOT/('baseline' if version=='0.0.4' else 'candidate')/'into-md.exe'
        command=[str(binary),*[i['inputPath'] for i in items],'--output-dir',str(out),'--report',str(report),'--jobs','1','--no-config','--ocr',mode,'--asset-mode','omit','--conflict','error','--log-format','json','--progress','never','--timeout-ms',str(120000*len(items))]
        dry=monitor(command+['--dry-run'],env,runroot/'dry-run',30)
        write(runroot/'dry-run.json',dry)
        if dry['exitCode']!=0:raise RuntimeError(f'Dry run failed: {runroot}')
    elif tool=='anydoc':command=['node',str(ROOT/'scripts/anydoc_runner.mjs'),str(group),str(out),str(report),str(ANYDOC)]
    else:command=[str(PYTHON),str(ROOT/'scripts/markitdown_runner.py'),str(group),str(out),str(report)]
    # Bounded batches; if a batch stops, every unreported input is retried alone.
    measurement=monitor(command,env,runroot/'conversion',150*len(items))
    measurement.update(tool=tool,group=seq,inputIds=[i['id'] for i in items],retry=single)
    write(runroot/'measurement.json',measurement)
    if tool.startswith('into-'):rows=native_results(report,items,out)
    else:
        try:rows=json.loads(report.read_text(encoding='utf-8'))['results']
        except (OSError,ValueError):rows=[]
    found={r['id'] for r in rows}
    if single or len(items)==1:
        rows.extend({'id':i['id'],'status':'failed','errorCode':measurement['termination'] or 'unreportedProcessFailure','error':f"exit {measurement['exitCode']}"} for i in items if i['id'] not in found)
    else:
        for item in items:
            if item['id'] not in found:rows.extend(execute(tool,[item],seq+'-retry-'+item['id'],True))
    for row in rows:row.setdefault('group',seq)
    write(runroot/'results.json',rows)
    return rows

def main():
    parser=argparse.ArgumentParser();parser.add_argument('tools',nargs='+');args=parser.parse_args()
    manifest=json.loads((ROOT/'manifest.json').read_text(encoding='utf-8'));items=manifest['items']
    for tool in args.tools:
        resultfile=ROOT/f'{tool}-results.json';rows=[]
        for n,group in enumerate(chunks(items)):
            seq=f'{n:04d}-{group[0]["extension"]}';existing=ROOT/'runs'/tool/seq/'results.json'
            if existing.exists():part=json.loads(existing.read_text(encoding='utf-8'))
            else:part=execute(tool,group,seq)
            rows.extend(part);write(resultfile,{'schemaVersion':1,'tool':tool,'results':rows,'complete':len(rows)==len(items)})
            print(f'{tool}: {len(rows)}/{len(items)} '+json.dumps(dict(collections.Counter(r['status'] for r in rows)))+' '+seq,flush=True)
        assert len({r['id'] for r in rows})==len(items)
if __name__=='__main__':main()
