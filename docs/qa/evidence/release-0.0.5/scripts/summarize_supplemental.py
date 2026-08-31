from pathlib import Path
import collections,json,statistics
import analyze

ROOT=Path(__file__).resolve().parents[1]

def main():
    records=json.loads((ROOT/'supplemental-results.json').read_text(encoding='utf-8'))
    paired=collections.defaultdict(lambda:collections.defaultdict(list));concurrency=[];fixed_memory=[];extended=[];kml=[]
    for record in records:
        m=record['measurement'];rows=record['results']
        if record['name'].startswith('paired-'):
            key=(m['ocr'],m['inputIds'][0]);paired[key][m['version']].append(record)
        elif record['name'].startswith('concurrency-'):
            concurrency.append({'version':m['version'],'jobs':m['jobs'],'inputs':len(m['inputIds']),'success':sum(v['status']=='success' for v in rows),'wallMs':m['wallMs'],'peakRssMiB':m['peakRssBytes']/2**20,'termination':m['termination']})
        elif record['name'].startswith('fixed-memory-'):
            fixed_memory.append({'version':m['version'],'ocr':m['ocr'],'jobs':m['jobs'],'memoryBudgetBytes':4294967296,'inputs':len(m['inputIds']),'success':sum(v['status']=='success' for v in rows),'wallMs':m['wallMs'],'peakRssMiB':m['peakRssBytes']/2**20,'termination':m['termination']})
        elif record['name'].startswith('extended-timeout-'):
            report=json.loads((ROOT/'supplemental'/record['name']/'report.json').read_text(encoding='utf-8-sig'))
            native=next(v for v in json.loads((ROOT/'into-0.0.4-off-results.json').read_text(encoding='utf-8'))['results'] if v['id']==m['inputIds'][0])
            baseline=analyze.grams(analyze.normalize(analyze.read_output(native) or ''))
            output=analyze.read_output(rows[0]) if rows and rows[0]['status']=='success' else None
            retention=len(baseline & analyze.grams(analyze.normalize(output)))/len(baseline) if baseline and output is not None else None
            extended.append({'version':m['version'],'id':m['inputIds'][0],'timeoutMs':m['internalTimeoutMs'],
                'wallMs':m['wallMs'],'statuses':[v['status'] for v in rows],'outputBytes':[v.get('outputBytes',0) for v in rows],
                'baselineNativeTextTrigramRetention':retention,'peakRssMiB':m['peakRssBytes']/2**20,
                'resourceUsage':report.get('resourceUsage',{}),'termination':m['termination']})
        else:
            kml.append({'case':record['name'],'exitCode':m['exitCode'],'statuses':[v['status'] for v in rows],'outputBytes':[v.get('outputBytes',0) for v in rows],'errorCodes':[v.get('errorCode') for v in rows]})
    timings=[]
    for (mode,ident),versions in paired.items():
        row={'id':ident,'ocr':mode,'repeats':3}
        for version,runs in versions.items():
            assert len(runs)==3
            row[version]={'medianWallMs':statistics.median(v['measurement']['wallMs'] for v in runs),'medianProcessingMs':statistics.median((v['results'][0].get('processingDurationMs') or 0) for v in runs),'statuses':[v['results'][0]['status'] if v['results'] else 'unreported' for v in runs],'peakRssMiB':max(v['measurement']['peakRssBytes'] for v in runs)/2**20}
        row['candidateToBaselineProcessingRatio']=row['0.0.5']['medianProcessingMs']/row['0.0.4']['medianProcessingMs'] if row['0.0.4']['medianProcessingMs'] else None
        timings.append(row)
    summary={'pairedTimings':timings,'concurrency':concurrency,'fixedMemory':fixed_memory,'extendedDeadline':extended,'kmlBoundary':kml,'method':'3 alternating version pairs per representative input; separate from the full-scan totals. Concurrency, fixed-4-GiB and extended-600-second runs are single observations, not full-corpus or statistical scaling benchmarks. The longer deadline never replaces the full-scan timeout outcome.'}
    (ROOT/'supplemental-summary.json').write_text(json.dumps(summary,ensure_ascii=False,indent=2),encoding='utf-8')
    if not (ROOT/'ocr-manual-reference.json').exists():
        print('Timing summaries written; private manual transcription is unavailable.',flush=True)
        return
    reference=json.loads((ROOT/'ocr-manual-reference.json').read_text(encoding='utf-8'));checks=[]
    for tool in analyze.TOOLS:
        rows={v['id']:v for v in json.loads((ROOT/f'{tool}-results.json').read_text(encoding='utf-8'))['results']}
        for ident,sample in reference['images'].items():
            row=rows[ident];text=analyze.normalize(analyze.read_output(row) or '')
            matches=[n for n,p in enumerate(sample['phrases']) if analyze.normalize(p,markup=False) in text]
            checks.append({'tool':tool,'id':ident,'status':row['status'],'referenceKind':sample['kind'],'expectedPhraseCount':len(sample['phrases']),'matchedPhraseCount':len(matches),'matchedPhraseIndices':matches,'errorCode':row.get('errorCode')})
    (ROOT/'ocr-manual-checks.json').write_text(json.dumps({'method':reference['method'],'checks':checks},ensure_ascii=False,indent=2),encoding='utf-8')
    print('Supplemental summaries written.',flush=True)

if __name__=='__main__':main()
