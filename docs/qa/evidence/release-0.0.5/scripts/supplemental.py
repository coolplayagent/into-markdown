from pathlib import Path
import json,statistics,zipfile
import run

ROOT=Path(__file__).resolve().parents[1]
OFF_IDS=['806cb0da70456d03','468c69e44f4774f5','cc54715a4a47401e']
OCR_IDS=['735a9c7e40f49351','3c05f4ee2ffbf99c','77447bb1944b2c95','4cf7e205361d7051','0e49f9f38550fe04']

def execute(version,mode,items,name,jobs=1,extra=(),timeout=120):
    root=ROOT/'supplemental'/name;root.mkdir(parents=True,exist_ok=True)
    report=root/'report.json';out=root/'outputs';out.mkdir(exist_ok=True)
    binary=ROOT/('baseline' if version=='0.0.4' else 'candidate')/'into-md.exe'
    environment=json.loads((ROOT/'environment.json').read_text(encoding='utf-8'))
    binary_sha=environment['baselineBinarySha256' if version=='0.0.4' else 'candidateBinarySha256']
    if (root/'results.json').exists() and (root/'measurement.json').exists():
        measurement=json.loads((root/'measurement.json').read_text(encoding='utf-8'))
        assert measurement['binarySha256']==binary_sha and measurement['jobs']==jobs and measurement['ocr']==mode
        assert measurement['inputIds']==[i['id'] for i in items]
        assert measurement.get('extraArguments',[])==list(extra)
        assert measurement.get('internalTimeoutMs',120000*len(items))==timeout*1000*len(items)
        return {'name':name,'measurement':measurement,'results':json.loads((root/'results.json').read_text(encoding='utf-8'))}
    assert not any(out.iterdir()),f'Preserve interrupted supplemental group before retrying: {root}'
    command=[str(binary),*[i['inputPath'] for i in items],'--output-dir',str(out),'--report',str(report),'--no-config','--jobs',str(jobs),'--ocr',mode,'--asset-mode','omit','--conflict','error','--log-format','json','--progress','never','--timeout-ms',str(timeout*1000*len(items)),*extra]
    measurement=run.monitor(command,run.environment('supplemental-'+version),root/'conversion',(timeout+30)*len(items))
    measurement.update(version=version,ocr=mode,inputIds=[i['id'] for i in items],jobs=jobs,binarySha256=binary_sha,extraArguments=list(extra),internalTimeoutMs=timeout*1000*len(items))
    run.write(root/'measurement.json',measurement)
    rows=run.native_results(report,items,out)
    if '--max-memory-size' in extra:
        actual=json.loads(report.read_text(encoding='utf-8-sig'))['resourceUsage']['sharedLeaseBudgetBytes']
        assert actual==int(extra[extra.index('--max-memory-size')+1]),actual
    run.write(root/'results.json',rows)
    return {'name':name,'measurement':measurement,'results':rows}

def main():
    for tool in ['into-0.0.4-off','into-0.0.4-auto','into-0.0.5-off','into-0.0.5-auto','anydoc','markitdown']:
        assert json.loads((ROOT/f'{tool}-results.json').read_text(encoding='utf-8'))['complete'],tool
    items={i['id']:i for i in json.loads((ROOT/'manifest.json').read_text(encoding='utf-8'))['items']}
    records=[]
    for mode,ids in [('off',OFF_IDS),('auto',OCR_IDS)]:
        for ident in ids:
            for repeat in range(3):
                order=['0.0.4','0.0.5'] if repeat%2==0 else ['0.0.5','0.0.4']
                for version in order:
                    name=f'paired-{mode}-{ident}-{repeat}-{version}'
                    records.append(execute(version,mode,[items[ident]],name))
                    run.write(ROOT/'supplemental-results.json',records)
                    print(name,records[-1]['measurement']['wallMs'],flush=True)
    for jobs in [1,4]:
        for version in ['0.0.4','0.0.5']:
            name=f'concurrency-auto-jobs{jobs}-{version}'
            records.append(execute(version,'auto',[items[i] for i in OCR_IDS],name,jobs))
            run.write(ROOT/'supplemental-results.json',records)
            print(name,records[-1]['measurement']['wallMs'],flush=True)
    for mode,ids in [('off',OFF_IDS),('auto',OCR_IDS)]:
        for version in ['0.0.4','0.0.5']:
            name=f'fixed-memory-4gib-{mode}-{version}'
            records.append(execute(version,mode,[items[i] for i in ids],name,extra=('--max-memory-size','4294967296')))
            run.write(ROOT/'supplemental-results.json',records)
            print(name,records[-1]['measurement']['wallMs'],flush=True)
    for version in ['0.0.4','0.0.5']:
        name=f'extended-timeout-auto-{version}'
        records.append(execute(version,'auto',[items['1840a79bdc5f6acf']],name,timeout=600))
        run.write(ROOT/'supplemental-results.json',records)
        print(name,records[-1]['measurement']['wallMs'],flush=True)
    # KML is an intentional unknown-extension admission boundary introduced by #339.
    ident='9e305ce381ae60fb';dest=ROOT/'supplemental'/'kml-source';dest.mkdir(exist_ok=True)
    with zipfile.ZipFile(items[ident]['inputPath']) as z:
        name=next(n for n in z.namelist() if n.endswith('.kml'))
        source=dest/'0000000000000000.kml';source.write_bytes(z.read(name))
    kml={'id':'0000000000000000','inputPath':str(source)}
    for version,extra in [('0.0.4',()),('0.0.5',()),('0.0.5',('--format','xml'))]:
        name=f'kml-{version}-'+('explicit-xml' if extra else 'automatic')
        records.append(execute(version,'off',[kml],name,extra=extra))
        run.write(ROOT/'supplemental-results.json',records)
        print(name,records[-1]['measurement']['exitCode'],flush=True)
    import summarize_supplemental
    summarize_supplemental.main()

if __name__=='__main__':main()
