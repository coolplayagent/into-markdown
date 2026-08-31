"""Fail when a successful baseline public sample loses output after the change."""
import json
import sys
from pathlib import Path
baseline, candidate = [json.loads(Path(path).read_text(encoding='utf-8')) for path in sys.argv[1:]]
before = {(case['sample']['kind'],case['sample']['name']):case for case in baseline['cases']}
after = {(case['sample']['kind'],case['sample']['name']):case for case in candidate['cases']}
assert before.keys() == after.keys() and len(before) >= 48
improved = []
for key, old in before.items():
    new = after[key]
    assert old['sample']['sha256'] == new['sample']['sha256'], key
    if old.get('markdown_bytes'):
        assert new.get('markdown_sha256') == old['markdown_sha256'], (key,'baseline output changed')
        assert new['assets'] == old['assets'], (key,'baseline assets changed')
    elif new.get('markdown_bytes'):
        improved.append('/'.join(key))
print(json.dumps({'compared':len(before),'newly_convertible':improved},ensure_ascii=False))
