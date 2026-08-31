"""Protect evidence accounting against public DTO origin and inline shapes."""

import base64
import unittest

from .observations import content


class ObservationTests(unittest.TestCase):
    def test_native_and_local_ocr_are_separate_and_assets_retain_bytes(self):
        def text(value, origin=None):
            data = {'value': value}
            if origin:
                data['provenance'] = {'kind': origin}
            return {'type': 'sourceText' if origin else 'text', 'data': data}

        def node(origin, inlines):
            return {'block': {'type': 'paragraph', 'data': inlines},
                    'provenance': {'kind': origin, 'locator': {'page': 2}}}

        result = {'markdown': 'body', 'document': {'blocks': [
            node('nativeParser', [text('native'), text('inlined OCR', 'localOcr')]),
            node('localOcr', [text('recognized')]), node('aiProvider', [text('generated')])]},
            'assets': [{'id': 'image', 'mediaType': 'image/png',
                        'dataBase64': base64.b64encode(b'original bytes').decode()}]}
        observed = content(result)
        self.assertEqual(observed['nativeUnits'][0]['characters'], len('native'))
        self.assertEqual(len(observed['ocrBlocks']), 1)
        self.assertEqual(observed['ocrBlocks'][0]['locator'], {'page': 2})
        self.assertEqual(len(observed['ocrInlines']), 1)
        self.assertEqual(observed['ocrInlines'][0]['characters'], len('inlined OCR'))
        self.assertEqual(observed['assetInventory'][0]['bytes'], 14)
        # Core serialization uses sourceText; the public DTO uses text with
        # provenance. Both must retain the same OCR and native inventories.
        result['document']['blocks'][0]['block']['data'][1]['type'] = 'text'
        public = content(result)
        for field in ('nativeUnits', 'ocrBlocks', 'ocrInlines', 'assetInventory'):
            self.assertEqual(public[field], observed[field])


if __name__ == '__main__':
    unittest.main()
