"""Repository-authored Drawio fixtures; no copied user diagrams or external assets."""
import base64
import zlib
from urllib.parse import quote

from fixture_expected import expected, expected_hash, limit_expected

MODEL = ('<mxGraphModel><root><mxCell id="0"/>'
         '<mxCell id="1" parent="0" value="Main"/>'
         '<mxCell id="a" parent="1" vertex="1" value="Start 中文"/>'
         '<mxCell id="b" parent="1" vertex="1" value="End"/>'
         '<mxCell id="e" parent="1" edge="1" source="a" target="b" value="Continue"/>'
         '</root></mxGraphModel>')
GOLDEN = "5ecb88e03eb41cad516db20588b7f68ad7f62c0f2e2bf69a3eddb4c4d4de3599"


def drawio_fixtures(root, make_fixture):
    compressor = zlib.compressobj(wbits=-15)
    compressed = compressor.compress(quote(MODEL, safe="~()*!.'").encode()) + compressor.flush()
    normal = f"<mxfile><diagram>{MODEL}</diagram></mxfile>".encode()
    encoded = b"<mxfile><diagram>" + base64.b64encode(compressed) + b"</diagram></mxfile>"
    definitions = [
        ("normal", "normal", normal, expected_hash("layer, two labeled nodes and one connection", GOLDEN)),
        ("compressed", "normal", encoded, expected_hash("compressed equivalent of normal diagram", GOLDEN)),
        ("bare", "normal", MODEL.encode(), expected_hash("bare equivalent of normal diagram", GOLDEN)),
        ("corrupt", "corrupt", b"<mxfile><diagram>broken!</diagram></mxfile>", expected("error", "invalid compressed page", error_code="malformed")),
        ("limit", "limit", normal, limit_expected("exact input byte boundary", "max_input_bytes", len(normal)-1, len(normal), "max_input_bytes", "", GOLDEN)),
    ]
    return [make_fixture(root, f"drawio-{name}", "drawio", scenario,
                         f"small/drawio/{name}.drawio", data,
                         "application/vnd.jgraph.mxfile", result)
            for name, scenario, data, result in definitions]
