from pathlib import Path
import collections,csv,hashlib,html,json,re,statistics,unicodedata

ROOT=Path(__file__).resolve().parents[1]
TOOLS=['into-0.0.4-off','into-0.0.5-off','into-0.0.4-auto','into-0.0.5-auto','anydoc','markitdown']
def write(name,value):
    (ROOT/name).write_text(json.dumps(value,ensure_ascii=False,indent=2),encoding='utf-8')
def plain(text):
    text=re.sub(r'<!--.*?-->',' ',text,flags=re.S)
    # Only remove actual supported HTML tags, never mathematical a < b ... > c.
    text=re.sub(r'</?(?:strong|em|b|i|u|s|del|ins|sub|sup|span|div|p|br|hr|table|thead|tbody|tfoot|tr|td|th|caption|colgroup|col|ul|ol|li|dl|dt|dd|h[1-6]|blockquote|pre|code|a|img|details|summary)(?:\s+[^<>]*?)?\s*/?>',' ',text,flags=re.I)
    text=re.sub(r'!\[[^\]]*\]\([^)]*\)',' ',text)
    text=re.sub(r'\[([^\]]*)\]\([^)]*\)',r'\1',text)
    return html.unescape(text)
def normalize(text,markup=True):return ''.join(c for c in unicodedata.normalize('NFKC',plain(text) if markup else text).lower() if c.isalnum())
def grams(text):return {text[n:n+3] for n in range(max(0,len(text)-2))}
def read_output(row):
    p=Path(row.get('output') or '_missing_')
    if not p.is_file():
        for marker in ['runs','supplemental']:
            if marker in p.parts:
                p=ROOT.joinpath(*p.parts[p.parts.index(marker):]);break
    if p.is_file() and p.stat().st_size<=32*1024**2:return p.read_text(encoding='utf-8',errors='replace')
    return None
def pct(values,q):
    if not values:return None
    v=sorted(values);return v[round((len(v)-1)*q)]
def quality(item,row,oracle):
    if not oracle or not oracle.get('nodes'):return {'scored':False,'reason':'no-independent-text-reference'}
    if row['status']!='success':return {'scored':False,'coverageAdjustedScore':0,'reason':row['status']}
    md=read_output(row)
    if md is None:return {'scored':False,'reason':'output-over-32MiB'}
    source=normalize('\n'.join(oracle['nodes']),markup=False);rendered=normalize(md)
    sg=grams(source);recall=len(sg & grams(rendered))/len(sg) if sg else 1
    cursor=ordered=total=0
    for node in oracle['nodes'][:5000]:
        value=normalize(node,markup=False)
        if len(value)<2:continue
        total+=len(value);found=rendered.find(value,cursor)
        if found>=0:ordered+=len(value);cursor=found+len(value)
    order=ordered/total if total else 1
    lines=md.splitlines();heads=sum(bool(re.match(r'^#{1,6}\s',v)) for v in lines)
    tables=md.lower().count('<tr')+sum(bool(re.match(r'^\s*\|.*\|\s*$',v)) for v in lines)
    for block in re.findall(r'```(?:tsv|csv)\s*\n(.*?)```',md,flags=re.S|re.I):
        tables+=sum(bool(line.strip()) for line in block.splitlines())
    table=1 if not oracle.get('tables') else min(1,tables/oracle['tables'])
    sections=1
    if item['extension'] in ['pptx','xlsx']:
        marks=heads+(len(re.findall(r'(?i)slide\s*\d+',md)) if item['extension']=='pptx' else md.count('```tsv'))
        sections=min(1,marks/max(1,oracle.get('sections',1)))
    clean=0 if sum(v.lstrip().startswith('```') for v in lines)%2 or any(ord(c)<32 and c not in '\n\r\t' for c in md) else 1
    score=100*(.55*recall+.2*order+.1*sections+.1*table+.05*clean)
    return {'scored':True,'textRecall':recall,'orderRecall':order,'sectionRecall':sections,'tableRecallProxy':table,'clean':clean,'qualityScore':score,'coverageAdjustedScore':score}
def main():
    manifest=json.loads((ROOT/'manifest.json').read_text(encoding='utf-8'));items=manifest['items'];by_id={i['id']:i for i in items}
    data={};summaries={};quality_rows={}
    for tool in TOOLS:
        f=ROOT/f'{tool}-results.json'
        if not f.exists():continue
        d=json.loads(f.read_text(encoding='utf-8'));rows=d['results'];data[tool]={r['id']:r for r in rows}
        measurements=[]
        for p in (ROOT/'runs'/tool).glob('*/measurement.json'):
            if p.parent.name=='smoke':continue
            measurements.append(json.loads(p.read_text(encoding='utf-8')))
        formats={}
        for ext in sorted({i['extension'] for i in items}):
            subset=[r for r in rows if by_id[r['id']]['extension']==ext]
            groups=[m for m in measurements if by_id[m['inputIds'][0]]['extension']==ext]
            formats[ext]={'inputs':len(subset),'counts':dict(collections.Counter(r['status'] for r in subset)),'wallSeconds':sum(m['wallMs'] for m in groups)/1000,'peakRssMiB':max([m['peakRssBytes']/1024**2 for m in groups] or [0])}
        qr=[]
        for row in rows:
            item=by_id[row['id']];oracle=None
            if item.get('oraclePath') and item.get('validity','').startswith('valid'):
                source=Path(item['oraclePath'])
                if not source.is_file():source=ROOT/'oracles'/(item['id']+'.json')
                oracle=json.loads(source.read_text(encoding='utf-8'))
            qr.append({'id':row['id'],'extension':item['extension'],**quality(item,row,oracle)})
        quality_rows[tool]=qr;scored=[r['qualityScore'] for r in qr if r.get('scored')]
        adjusted=[r['coverageAdjustedScore'] for r in qr if r.get('coverageAdjustedScore') is not None]
        independent=[r for r in rows if by_id[r['id']].get('validity','').startswith('valid')]
        summaries[tool]={'complete':d['complete'],'inputs':len(rows),'counts':dict(collections.Counter(r['status'] for r in rows)),'wallSeconds':sum(m['wallMs'] for m in measurements)/1000,'peakRssMiB':max([m['peakRssBytes']/1024**2 for m in measurements] or [0]),'terminatedGroups':sum(bool(m['termination']) for m in measurements),'independentValidCohort':len(independent),'independentValidNonempty':sum(r['status']=='success' for r in independent),'qualityScored':len(scored),'qualityMedian':statistics.median(scored) if scored else None,'qualityP10':pct(scored,.1),'coverageAdjustedQualityMean':statistics.mean(adjusted) if adjusted else None,'formats':formats}
        print(tool,summaries[tool]['counts'],flush=True)
    changes={}
    for mode in ['off','auto']:
        a=data.get('into-0.0.4-'+mode,{});b=data.get('into-0.0.5-'+mode,{})
        qa={r['id']:r for r in quality_rows.get('into-0.0.4-'+mode,[])};qb={r['id']:r for r in quality_rows.get('into-0.0.5-'+mode,[])}
        changed=[]
        for ident in a.keys()&b.keys():
            old,new=a[ident],b[ident];flags=[]
            if old['status']=='success' and new['status']!='success':flags.append('success-lost')
            if old['status']!='success' and new['status']=='success':flags.append('success-gained')
            if old['status']==new['status']=='success':
                oldms=old.get('processingDurationMs') or 0;newms=new.get('processingDurationMs') or 0
                if oldms>=100 and newms>oldms*2 and newms-oldms>500:flags.append('time-over-2x')
                q0=qa.get(ident,{});q1=qb.get(ident,{})
                if q0.get('scored') and q1.get('scored') and q0['textRecall']-q1['textRecall']>.1:flags.append('text-recall-down-10pp')
                if old.get('outputBytes',0)>200 and new.get('outputBytes',0)<old['outputBytes']*.6:flags.append('output-bytes-down-40pct')
            if flags:changed.append({'id':ident,'path':by_id[ident]['relativePath'],'flags':flags,'beforeStatus':old['status'],'afterStatus':new['status'],'beforeMs':old.get('processingDurationMs'),'afterMs':new.get('processingDurationMs'),'beforeBytes':old.get('outputBytes'),'afterBytes':new.get('outputBytes'),'beforeError':old.get('errorCode'),'afterError':new.get('errorCode')})
        changes[mode]=changed
    ocr_deltas=[]
    old_auto=data.get('into-0.0.4-auto',{});old_off=data.get('into-0.0.4-off',{})
    new_auto=data.get('into-0.0.5-auto',{})
    for ident in old_auto.keys()&old_off.keys()&new_auto.keys():
        before=old_auto[ident];without=old_off[ident];after=new_auto[ident]
        if before['status']!='success':continue
        auto_md=read_output(before);off_md=read_output(without) if without['status']=='success' else ''
        after_md=read_output(after) if after['status']=='success' else ''
        if auto_md is None or off_md is None or after_md is None or auto_md==off_md:continue
        contribution=grams(normalize(auto_md))-grams(normalize(off_md))
        if len(contribution)<50:continue
        retained=len(contribution&grams(normalize(after_md)))/len(contribution)
        ocr_deltas.append({'id':ident,'path':by_id[ident]['relativePath'],'baselineExtraTrigrams':len(contribution),'retainedByCandidateAuto':retained,'review':retained<.75})
    write('ocr-contribution-comparison.json',ocr_deltas)
    write('summary.json',summaries);write('quality-results.json',quality_rows);write('regression-candidates.json',changes)
    if set(data)==set(TOOLS) and all(s['complete'] for s in summaries.values()):
        scored={tool:{r['id']:r for r in rows if r.get('scored')} for tool,rows in quality_rows.items()}
        common=set.intersection(*(set(rows) for rows in scored.values()))
        timed={ident for ident in common if all(isinstance(data[t][ident].get('processingDurationMs'),(int,float)) for t in TOOLS)}
        comparison={'qualityInputCount':len(common),'timedInputCount':len(timed),'inputIds':sorted(timed),
            'method':'The same independently referenced, nonempty, scorable inputs for every tool. Processing times exclude process startup and final report writes; this successful intersection is not an all-input throughput result. OCR modes may perform additional work.',
            'tools':{t:{'qualityMedian':statistics.median(scored[t][i]['qualityScore'] for i in common) if common else None,
                'processingSeconds':sum(data[t][i]['processingDurationMs'] for i in timed)/1000,
                'medianProcessingMs':statistics.median(data[t][i]['processingDurationMs'] for i in timed) if timed else None} for t in TOOLS}}
        write('common-cohort.json',comparison)
    with (ROOT/'per-file.csv').open('w',encoding='utf-8-sig',newline='') as f:
        w=csv.writer(f);w.writerow(['tool','id','relativePath','extension','sha256','status','processingDurationMs','outputBytes','errorCode'])
        for tool,rows in data.items():
            for ident,row in rows.items():
                i=by_id[ident];w.writerow([tool,ident,i['relativePath'],i['extension'],i['sha256'],row['status'],row.get('processingDurationMs'),row.get('outputBytes'),row.get('errorCode')])
if __name__=='__main__':main()
