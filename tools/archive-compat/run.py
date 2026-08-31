"""CLI corpus and safety acceptance for source builds and extracted release executables."""
from __future__ import annotations
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import time
import zipfile
from samples import fetch, sha256


def run_process(command, cwd, log):
    system = platform.system()
    if system == 'Darwin':
        command = ['/usr/bin/time', '-l', *command]
    elif system == 'Linux' and Path('/usr/bin/time').exists():
        command = ['/usr/bin/time', '-v', *command]
    started = time.monotonic()
    peak = 0
    with log.open('wb') as output:
        process = subprocess.Popen(command, cwd=cwd, stdout=output, stderr=subprocess.STDOUT)
        if system == 'Windows':
            from ctypes import wintypes
            class Counters(ctypes.Structure):
                _fields_ = [('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD)] + [(name, ctypes.c_size_t) for name in ['PeakWorkingSetSize', 'WorkingSetSize', 'QuotaPeakPagedPoolUsage', 'QuotaPagedPoolUsage', 'QuotaPeakNonPagedPoolUsage', 'QuotaNonPagedPoolUsage', 'PagefileUsage', 'PeakPagefileUsage']]
            get_memory = ctypes.windll.psapi.GetProcessMemoryInfo
            get_memory.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
            while process.poll() is None:
                counters = Counters(); counters.cb = ctypes.sizeof(counters)
                if get_memory(wintypes.HANDLE(int(process._handle)), ctypes.byref(counters), counters.cb):
                    peak = max(peak, counters.PeakWorkingSetSize)
                if time.monotonic() - started > 180:
                    process.kill(); process.wait(); raise TimeoutError(command)
                time.sleep(0.025)
        try:
            code = process.wait(timeout=180)
        except subprocess.TimeoutExpired:
            process.kill(); process.wait(); raise
    elapsed = time.monotonic() - started
    text = log.read_text(encoding='utf-8', errors='replace')
    if system == 'Darwin':
        found = re.search(r'(\d+)\s+maximum resident set size', text)
        peak = int(found[1]) if found else 0
    elif system == 'Linux':
        found = re.search(r'Maximum resident set size \(kbytes\):\s*(\d+)', text)
        peak = int(found[1])*1024 if found else 0
    return dict(exit_code=code, elapsed_seconds=elapsed, peak_rss_bytes=peak or None, log=text[:6000])


def check_assets(markdown, output):
    links = re.findall(r'!\[[^\]]*\]\(<?([^\s)>]+)>?\)', markdown)
    from urllib.parse import unquote
    checked = []
    for link in links:
        if link.startswith(('data:', 'https:', 'http:')):
            continue
        path = (output.parent / unquote(link)).resolve()
        path.relative_to(output.parent.resolve())
        if not path.is_file():
            raise AssertionError(f'missing asset {link}')
        checked.append(dict(path=path.relative_to(output.parent).as_posix(), sha256=sha256(path.read_bytes())))
    return checked


def execute(binary, source, work, extra=()):
    work.mkdir(parents=True, exist_ok=True)
    output = work / '结果.md'
    command = [str(binary), str(source), '--no-config', '--progress', 'never', '--ocr', 'off', '--error-policy', 'best-effort', '--asset-mode', 'extract', '--conflict', 'overwrite', '--report', str(work/'batch.json'), '-o', str(output), *extra]
    result = run_process(command, work, work / 'process.log')
    result['command'] = command
    if (work/'batch.json').exists():
        result['batch'] = json.loads((work/'batch.json').read_text(encoding='utf-8'))
    if output.exists():
        data = output.read_bytes()
        result.update(markdown_sha256=sha256(data), markdown_bytes=len(data), assets=check_assets(data.decode('utf-8'), output))
    return result


def make_zip(path, entries):
    with zipfile.ZipFile(path, 'w', compression=zipfile.ZIP_STORED) as archive:
        for name, value in entries:
            archive.writestr(name, value)


def presentation(path, runs=100_001):
    p = 'http://schemas.openxmlformats.org/presentationml/2006/main'
    a = 'http://schemas.openxmlformats.org/drawingml/2006/main'
    r = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
    rel = 'http://schemas.openxmlformats.org/package/2006/relationships'
    slide = f'<p:sld xmlns:p="{p}" xmlns:a="{a}"><p:cSld><p:spTree><p:sp><p:nvSpPr><p:cNvPr id="1" name="Body"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p>' + '<a:r><a:t>kept </a:t></a:r>'*runs + '</a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>'
    make_zip(path, [
        ('[Content_Types].xml', '<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/><Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/></Types>'),
        ('_rels/.rels', f'<Relationships xmlns="{rel}"><Relationship Id="rId1" Type="{r}/officeDocument" Target="ppt/presentation.xml"/></Relationships>'),
        ('ppt/presentation.xml', f'<p:presentation xmlns:p="{p}" xmlns:r="{r}"><p:sldIdLst><p:sldId id="256" r:id="rId1"/></p:sldIdLst><p:sldSz cx="9144000" cy="6858000"/></p:presentation>'),
        ('ppt/_rels/presentation.xml.rels', f'<Relationships xmlns="{rel}"><Relationship Id="rId1" Type="{r}/slide" Target="slides/slide1.xml"/></Relationships>'),
        ('ppt/slides/slide1.xml', slide),
    ])


def synthetic(binary, root):
    root.mkdir(parents=True, exist_ok=True)
    cases = []
    for version, signature in [(4,b'Rar!\x1a\x07\x00'), (5,b'Rar!\x1a\x07\x01\x00')]:
        source = root / f'RAR{version}.txt'; source.write_bytes(signature)
        result = execute(binary, source, root / f'rar{version}')
        assert result['exit_code'] != 0 and 'unsupported' in result['log'] and 'extract' in result['log'], result
        cases.append(dict(name=f'rar{version}', **result))
    source = root / '混合归档.zip'
    make_zip(source, [('报告（最终）.txt','完整正文'), ('伪装.zip',b'Rar!\x1a\x07\x01\x00')])
    result = execute(binary, source, root / 'mixed')
    assert result.get('markdown_bytes') and '完整正文' in (root/'mixed/结果.md').read_text(encoding="utf-8"), result
    assert 'extract' in json.dumps(result.get('batch',{})), result
    cases.append(dict(name='mixed', **result))
    for name, entries in [('unicode-alias',[('é.txt','a'),('e\u0301.txt','b')]),('case-prefix',[('A/x.txt','a'),('a/y.txt','b')]),('traversal',[('../escape.txt','a')]),('device',[('ＣＯＮ.txt','a')])]:
        source = root / f'{name}.zip'; make_zip(source, entries)
        result = execute(binary, source, root/name)
        assert result['exit_code'] != 0 and not result.get('markdown_bytes'), result
        cases.append(dict(name=name, **result))
    source = root/'复杂幻灯片.pptx'; presentation(source)
    result = execute(binary, source, root/'pptx')
    assert result.get('markdown_bytes'), result
    assert (root/'pptx/结果.md').read_text(encoding="utf-8").count('kept') == 100_001
    cases.append(dict(name='high-complexity', **result))
    result = execute(binary, source, root/'pptx-low', ['--max-presentation-xml-events','100000'])
    assert result['exit_code'] != 0 and 'max_presentation_xml_events' in result['log'] and not result.get('markdown_bytes'), result
    cases.append(dict(name='event-budget', **result))
    return cases


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--into-md', type=Path, required=True)
    parser.add_argument('--work-root', type=Path, required=True)
    parser.add_argument('--cache', type=Path)
    parser.add_argument('--public-samples', action='store_true')
    parser.add_argument('--baseline', action='store_true')
    args = parser.parse_args()
    binary = args.into_md.resolve(); root = args.work_root.resolve(); root.mkdir(parents=True, exist_ok=True)
    report = dict(schema_version=1, platform=platform.platform(), binary_sha256=sha256(binary.read_bytes()), version=subprocess.check_output([str(binary),'version','--json','--no-config'],text=True), cases=[])
    if not args.baseline:
        report['synthetic'] = synthetic(binary, root/'synthetic')
    if args.public_samples:
        cache = (args.cache or root/'public').resolve()
        for sample in fetch(cache):
            extra = ['--zip-charset',sample['charset']] if sample.get('charset') else []
            result = execute(binary, cache/sample['kind']/sample['name'], root/sample['kind']/sample['name'], extra)
            if sample['kind'] == 'rar' and not args.baseline:
                assert result['exit_code'] != 0 and 'unsupported' in result['log'] and 'extract' in result['log'], result
            report['cases'].append(dict(sample=sample, **result))
            (root/'report.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n', encoding='utf-8')
            print(sample['kind'], sample['name'], result['exit_code'], result.get('markdown_bytes',0),flush=True)
    (root/'report.json').write_text(json.dumps(report,ensure_ascii=False,indent=2)+'\n', encoding='utf-8')

if __name__ == '__main__':
    main()
