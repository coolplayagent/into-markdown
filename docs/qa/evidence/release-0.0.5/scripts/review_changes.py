from pathlib import Path
import collections,difflib,json,re
import analyze

ROOT=Path(__file__).resolve().parents[1]

def main():
    manifest=json.loads((ROOT/'manifest.json').read_text(encoding='utf-8'))
    items={i['id']:i for i in manifest['items']}
    candidates=json.loads((ROOT/'regression-candidates.json').read_text(encoding='utf-8'))
    review={}
    for mode,changes in candidates.items():
        files=[ROOT/f'into-{version}-{mode}-results.json' for version in ['0.0.4','0.0.5']]
        if not all(p.exists() for p in files):continue
        old,new=[{i['id']:i for i in json.loads(p.read_text(encoding='utf-8'))['results']} for p in files]
        checked=[]
        for c in changes:
            if c['id'] not in new:continue
            a,b=old[c['id']],new[c['id']]
            entry=dict(c)
            if a['status']==b['status']=='success':
                before,after=analyze.read_output(a),analyze.read_output(b)
                if before is not None and after is not None:
                    def without_empty_notes(text):
                        return re.sub(r'^### Speaker notes\s*$(?=\s*(?:## Slide |\Z))','',text,flags=re.M)
                    x,y=map(analyze.normalize,[without_empty_notes(before),without_empty_notes(after)])
                    entry['normalizedTextEqualIgnoringEmptyNotes']=x==y
                    if x!=y and max(len(x),len(y))<200000:
                        # A diagnostic diff, not an edit-distance quality score.
                        # Popular-token suppression avoids quadratic work on repeated tables.
                        ops=difflib.SequenceMatcher(None,x,y,autojunk=True).get_opcodes()
                        entry['diffHeuristic']='SequenceMatcher with popular-token suppression; normalized equality remains exact'
                        entry['changedAlphanumericCharacters']=sum(max(j-i,l-k) for op,i,j,k,l in ops if op!='equal')
                        # Full text remains local/private; the public review only records counts.
                        private=ROOT/'review-private'/f'{mode}-{c["id"]}.json'
                        private.parent.mkdir(exist_ok=True)
                        private.write_text(json.dumps([{'operation':op,'before':x[i:j],'after':y[k:l]} for op,i,j,k,l in ops if op!='equal'],ensure_ascii=False,indent=2),encoding='utf-8')
            checked.append(entry)
        review[mode]=checked
        print(mode,len(checked),collections.Counter(flag for v in checked for flag in v['flags']), 'normalized-equal',sum(v.get('normalizedTextEqualIgnoringEmptyNotes',False) for v in checked),flush=True)
    (ROOT/'regression-review.json').write_text(json.dumps(review,ensure_ascii=False,indent=2),encoding='utf-8')

if __name__=='__main__':main()
