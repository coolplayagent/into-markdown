#!/usr/bin/env python3
"""Build a masked-image OCR contract from the frozen English image golden.

Requires Pillow locally. The page contains native text to exercise image OCR
when the scan-only page heuristic does not apply. The visible sentence is stored
in a PDF soft mask; the underlying image pixels are uniformly black.
"""
import argparse
import hashlib
import json
from pathlib import Path
import zlib

ROOT = Path(__file__).resolve().parents[2]


def stream(meta, data):
    data = zlib.compress(data)
    return f'<< {meta} /Filter /FlateDecode /Length {len(data)} >>\nstream\n'.encode() + data + b'\nendstream'


def assemble(objects):
    data = bytearray(b'%PDF-1.4\n')
    offsets = []
    for index, obj in enumerate(objects):
        offsets.append(len(data))
        data.extend(f'{index + 1} 0 obj\n'.encode() + obj + b'\nendobj\n')
    start = len(data)
    data.extend(f'xref\n0 {len(objects) + 1}\n0000000000 65535 f \n'.encode())
    for offset in offsets:
        data.extend(f'{offset:010} 00000 n \n'.encode())
    data.extend(f'trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\nstartxref\n{start}\n%%EOF\n'.encode())
    return bytes(data)


def build(output):
    from PIL import Image, ImageOps

    fixtures = json.loads((ROOT / 'fixtures/manifest.json').read_text())
    fixture = next(x for x in fixtures['fixtures'] if x['id'] == 'ocr-english-clear-1')
    golden = next(x for x in fixtures['ocr_quality']['goldens'] if x['fixture_id'] == fixture['id'])
    original = ROOT / 'fixtures' / fixture['path']
    assert hashlib.sha256(original.read_bytes()).hexdigest() == golden['fixture_sha256']
    with Image.open(original) as image:
        gray = image.convert('L')
    width, height = gray.size
    objects = [
        b'<< /Type /Catalog /Pages 2 0 R >>',
        b'<< /Type /Pages /Kids [3 0 R] /Count 1 >>',
        b'<< /Type /Page /Parent 2 0 R /MediaBox [0 0 640 400] /Resources << /Font << /F1 5 0 R >> /XObject << /Im1 6 0 R >> >> /Contents 4 0 R >>',
        stream('', f'BT /F1 12 Tf 20 350 Td (Native container text.) Tj ET\nq 600 0 0 {600 * height / width} 20 150 cm /Im1 Do Q'.encode()),
        b'<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>',
        stream(f'/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 7 0 R', bytes(width * height)),
        stream(f'/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8', ImageOps.invert(gray).tobytes()),
    ]
    output.mkdir(parents=True, exist_ok=False)
    source = output / 'soft-mask-text.pdf'
    source.write_bytes(assemble(objects))
    sha = hashlib.sha256(source.read_bytes()).hexdigest()
    paper = dict(id=source.stem, sourcePath=source.name, title='Text encoded in a soft mask',
                 format='pdf', pages=1, bytes=source.stat().st_size, sha256=sha, sourceClass='valid')
    (output / 'manifest.json').write_text(json.dumps({'papers': [paper]}, indent=2) + '\n')
    cases = []
    for mode in ('auto', 'off'):
        case = dict(id=source.stem, ocr=mode, sourceSha256=sha, requiredText=['Native container text.'])
        if mode == 'auto':
            case['requiredText'].append(golden['ground_truth_nfc'])
            case['exactTextCounts'] = {golden['ground_truth_nfc']: 1}
        cases.append(case)
    (output / 'expectations.json').write_text(json.dumps({'schemaVersion': 1, 'cases': cases}, indent=2) + '\n')
    (output / 'authority.json').write_text(json.dumps(dict(imageSha256=golden['fixture_sha256'],
        imageOnlyText=golden['ground_truth_nfc'], sourceSha256=sha), indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    build(parser.parse_args().output)
