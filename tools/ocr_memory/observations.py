"""Content fingerprints for frozen OCR evidence, without republishing source text."""

import base64
import hashlib
import json
from collections import defaultdict


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def inline_text(value, native):
    if isinstance(value, list):
        return ''.join(inline_text(item, native) for item in value)
    if not isinstance(value, dict):
        return ''
    data = value.get('data', {})
    if value.get('type') in {'text', 'sourceText'} and isinstance(data, dict):
        origin = data.get('provenance', {}).get('kind')
        if origin in {'localOcr', 'aiProvider'}:
            return ''
        return data.get('value', '') if native else ''
    if value.get('type') == 'link' and isinstance(data, dict):
        return inline_text(data.get('content', []), native)
    return ''


def content(result):
    units = defaultdict(list)
    ocr_units = []
    ocr_inlines = []

    def visit(value):
        if isinstance(value, list):
            for item in value:
                visit(item)
        elif isinstance(value, dict):
            if value.get('type') in {'text', 'sourceText'}:
                data = value.get('data', {})
                origin = data.get('provenance', {})
                if origin.get('kind') == 'localOcr':
                    ocr_inlines.append({'locator': origin.get('locator'),
                        'characters': len(data.get('value', '')), 'sha256': digest(data)})
            if 'block' in value and 'provenance' in value:
                block, origin = value['block'], value['provenance']
                data, kind = block.get('data', {}), block.get('type')
                native = origin.get('kind') == 'nativeParser'
                text = ''
                if kind == 'paragraph':
                    text = inline_text(data, native)
                elif kind == 'heading':
                    text = inline_text(data.get('content', []), native)
                elif kind == 'code' and native:
                    text = data.get('text', '')
                if text:
                    locator = json.dumps(origin.get('locator', {}), sort_keys=True)
                    units[locator].append(text)
                if origin.get('kind') == 'localOcr':
                    ocr_units.append({'locator': origin.get('locator'), 'sha256': digest(block)})
            for child in value.values():
                if isinstance(child, (dict, list)):
                    visit(child)

    visit(result.get('document', {}))
    assets = []
    for item in result.get('assets', []):
        raw = base64.b64decode(item.get('dataBase64') or '', validate=True)
        assets.append({'id': item['id'], 'mediaType': item['mediaType'], 'bytes': len(raw),
                       'sha256': hashlib.sha256(raw).hexdigest(), 'externalUri': item.get('externalUri')})
    return {'nativeUnits': [{'locator': json.loads(key), 'characters': sum(map(len, texts)),
                             'sha256': digest(texts)} for key, texts in sorted(units.items())],
            # Block and inline views may overlap. Use the runtime's merged
            # region/character counters for totals, never sum these inventories.
            'assetInventory': assets, 'ocrBlocks': ocr_units, 'ocrInlines': ocr_inlines,
            'diagnostics': result.get('diagnostics', []),
            'documentSha256': digest(result.get('document')),
            'markdownSha256': hashlib.sha256(result['markdown'].encode()).hexdigest()}
