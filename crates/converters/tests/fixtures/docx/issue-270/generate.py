#!/usr/bin/env python3
"""Generate deterministic, repository-authored DOCX issue #270 fixtures."""

from __future__ import annotations

import base64
import hashlib
import json
import pathlib
import zipfile


ROOT = pathlib.Path(__file__).resolve().parent
WORD = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
REL = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
PACKAGE_REL = "http://schemas.openxmlformats.org/package/2006/relationships"
ALT_REL = f"{REL}/aFChunk"
STRICT_WORD = "http://purl.oclc.org/ooxml/wordprocessingml/main"
STRICT_REL = "http://purl.oclc.org/ooxml/officeDocument/relationships"
PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
)


def package(
    name: str,
    document: str,
    parts: dict[str, tuple[str, bytes]],
    rels: str = "",
    *,
    stored: bool = False,
    office_relationship_namespace: str = REL,
) -> None:
    overrides = [
        '<Override PartName="/word/document.xml" '
        'ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
    ]
    for path, (media_type, _) in parts.items():
        overrides.append(f'<Override PartName="/{path}" ContentType="{media_type}"/>')
    types = (
        '<?xml version="1.0"?><Types '
        'xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
        + "".join(overrides)
        + "</Types>"
    )
    root_rels = (
        f'<Relationships xmlns="{PACKAGE_REL}"><Relationship Id="root" Type="{office_relationship_namespace}/officeDocument" '
        'Target="word/document.xml"/></Relationships>'
    )
    document_rels = f'<Relationships xmlns="{PACKAGE_REL}">{rels}</Relationships>'
    destination = ROOT / name
    compression = zipfile.ZIP_STORED if stored else zipfile.ZIP_DEFLATED
    with zipfile.ZipFile(destination, "w", compression) as archive:
        for path, data in sorted(
            {
                "[Content_Types].xml": types.encode(),
                "_rels/.rels": root_rels.encode(),
                "word/document.xml": document.encode(),
                "word/_rels/document.xml.rels": document_rels.encode(),
                **{path: data for path, (_, data) in parts.items()},
            }.items()
        ):
            info = zipfile.ZipInfo(path, (1980, 1, 1, 0, 0, 0))
            info.compress_type = compression
            info.external_attr = 0o100644 << 16
            archive.writestr(info, data)


def document(
    body: str,
    extra_namespaces: str = "",
    *,
    word_namespace: str = WORD,
    relationship_namespace: str = REL,
) -> str:
    return f'<w:document xmlns:w="{word_namespace}" xmlns:r="{relationship_namespace}" {extra_namespaces}><w:body>{body}</w:body></w:document>'


def paragraph(text: str) -> str:
    return f"<w:p><w:r><w:t>{text}</w:t></w:r></w:p>"


def alt_fixture(name: str, target: str, media_type: str, payload: bytes) -> None:
    body = paragraph("before") + '<w:altChunk r:id="chunk"/>' + paragraph("after")
    relation = f'<Relationship Id="chunk" Type="{ALT_REL}" Target="{target.removeprefix("word/")}"/>'
    package(name, document(body), {target: (media_type, payload)}, relation)


def main() -> None:
    alt_fixture("html.docx", "word/chunk.html", "text/html", b"<html><body><script>script-hidden</script><h2>HTML heading</h2><p>HTML visible</p></body></html>")
    alt_fixture("xhtml.docx", "word/chunk.xhtml", "application/xhtml+xml", b'<html xmlns="http://www.w3.org/1999/xhtml"><body><p>XHTML visible</p></body></html>')
    alt_fixture("rtf.docx", "word/chunk.rtf", "application/rtf", br"{\rtf1\ansi RTF visible}")
    mhtml = b'MIME-Version: 1.0\r\nContent-Type: multipart/related; boundary="safe"\r\n\r\n--safe\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<html><body><p>MHTML=20visible</p></body></html>\r\n--safe--\r\n'
    alt_fixture("mhtml.docx", "word/chunk.mht", "message/rfc822", mhtml)
    strict_relation = (
        f'<Relationship Id="chunk" Type="{STRICT_REL}/aFChunk" Target="chunk.html"/>'
    )
    package(
        "strict.docx",
        document(
            paragraph("strict-before")
            + '<w:altChunk r:id="chunk"/>'
            + paragraph("strict-after"),
            word_namespace=STRICT_WORD,
            relationship_namespace=STRICT_REL,
        ),
        {"word/chunk.html": ("text/html", b"<p>strict chunk</p>")},
        strict_relation,
        office_relationship_namespace=STRICT_REL,
    )
    external = '<Relationship Id="chunk" Type="{}" Target="https://127.0.0.1/private" TargetMode="External"/>'.format(ALT_REL)
    package("external.docx", document(paragraph("before") + '<w:altChunk r:id="chunk"/>' + paragraph("after")), {}, external)
    cycle = f'<Relationship Id="chunk" Type="{ALT_REL}" Target="document.xml"/>'
    package("cycle.docx", document('<w:altChunk r:id="chunk"/>'), {}, cycle)
    alt_fixture("entity.docx", "word/chunk.xhtml", "application/xhtml+xml", b'<!DOCTYPE html [<!ENTITY secret SYSTEM "file:///secret">]><html xmlns="http://www.w3.org/1999/xhtml"><body>&secret;</body></html>')
    deep = paragraph("inside")
    for _ in range(260):
        deep = f"<w:sdt><w:sdtContent>{deep}</w:sdtContent></w:sdt>"
    package("depth-limit.docx", document(deep), {}, stored=True)
    empty = document("")
    package("empty.docx", empty, {})
    wrappers_body = (
        paragraph("wrapper-before")
        + f'<w:sdt><w:sdtContent>{paragraph("content-control")}</w:sdtContent></w:sdt>'
        + f'<w:customXml>{paragraph("custom-xml-wrapper")}</w:customXml>'
        + '<mc:AlternateContent><mc:Choice Requires="unsupported">'
        + paragraph("choice-hidden")
        + '</mc:Choice><mc:Fallback>'
        + paragraph("compatibility-fallback")
        + '</mc:Fallback></mc:AlternateContent>'
        + '<w:p><w:fldSimple w:instr="PAGE"><w:r><w:t>field-result</w:t></w:r></w:fldSimple></w:p>'
        + '<w:p><w:r><w:pict><v:shape><v:textbox><w:txbxContent>'
        + paragraph("textbox-visible")
        + '</w:txbxContent></v:textbox></v:shape></w:pict></w:r></w:p>'
        + '<w:sectPr><w:headerReference r:id="header"/><w:footerReference r:id="footer"/></w:sectPr>'
        + paragraph("wrapper-after")
    )
    wrappers_rels = (
        f'<Relationship Id="header" Type="{REL}/header" Target="header1.xml"/>'
        f'<Relationship Id="footer" Type="{REL}/footer" Target="footer1.xml"/>'
    )
    package(
        "wrappers.docx",
        document(
            wrappers_body,
            f'xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:unsupported="urn:unsupported" xmlns:v="urn:schemas-microsoft-com:vml"',
        ),
        {
            "word/header1.xml": (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml",
                f'<w:hdr xmlns:w="{WORD}">{paragraph("header-visible")}</w:hdr>'.encode(),
            ),
            "word/footer1.xml": (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml",
                f'<w:ftr xmlns:w="{WORD}">{paragraph("footer-visible")}</w:ftr>'.encode(),
            ),
        },
        wrappers_rels,
    )
    table = (
        "<w:tbl><w:tr><w:tc><w:tcPr><w:vMerge w:val=\"restart\"/></w:tcPr>"
        + paragraph("outer-a")
        + "<w:tbl><w:tr><w:tc>"
        + paragraph("nested")
        + "</w:tc></w:tr></w:tbl></w:tc><w:tc>"
        + paragraph("outer-b")
        + "</w:tc></w:tr><w:tr><w:tc><w:tcPr><w:vMerge/></w:tcPr>"
        + paragraph("merged")
        + "</w:tc><w:tc>"
        + paragraph("outer-c")
        + "</w:tc></w:tr></w:tbl>"
    )
    drawing_a = '<w:p><w:r><w:drawing><a:blip r:embed="image-a"/></w:drawing></w:r></w:p>'
    ordered_body = (
        paragraph("body-before")
        + '<w:p><w:hyperlink r:id="link"><w:r><w:t>linked</w:t></w:r></w:hyperlink></w:p>'
        + drawing_a
        + table
        + '<w:altChunk r:id="chunk"/>'
        + '<w:p><w:r><w:footnoteReference w:id="1"/></w:r></w:p>'
        + paragraph("body-after")
    )
    ordered_rels = (
        f'<Relationship Id="chunk" Type="{ALT_REL}" Target="chunk.html"/>'
        f'<Relationship Id="image-a" Type="{REL}/image" Target="media/pixel.png"/>'
        f'<Relationship Id="image-b" Type="{REL}/image" Target="media/pixel.png"/>'
        f'<Relationship Id="link" Type="{REL}/hyperlink" Target="https://example.invalid/" TargetMode="External"/>'
        f'<Relationship Id="notes" Type="{REL}/footnotes" Target="footnotes.xml"/>'
    )
    package(
        "ordered-nested-merged.docx",
        document(ordered_body, 'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'),
        {
            "word/chunk.html": ("text/html", b"<p>chunk-middle</p><p>chunk-middle</p>"),
            "word/media/pixel.png": ("image/png", PNG),
            "word/footnotes.xml": (
                "application/vnd.openxmlformats-officedocument.wordprocessingml.footnotes+xml",
                f'<w:footnotes xmlns:w="{WORD}"><w:footnote w:id="1">{paragraph("footnote-after-ref")}</w:footnote></w:footnotes>'.encode(),
            ),
        },
        ordered_rels,
    )
    drawing_one = '<w:p><w:r><w:drawing><a:blip r:embed="image-one"/></w:drawing></w:r></w:p>'
    drawing_two = '<w:p><w:r><w:drawing><a:blip r:embed="image-two"/></w:drawing></w:r></w:p>'
    duplicate_body = (
        paragraph("repeat")
        + drawing_one
        + paragraph("repeat")
        + drawing_two
        + '<w:altChunk r:id="one"/><w:altChunk r:id="two"/>'
    )
    duplicate_rels = (
        f'<Relationship Id="one" Type="{ALT_REL}" Target="one.html"/>'
        f'<Relationship Id="two" Type="{ALT_REL}" Target="two.html"/>'
        f'<Relationship Id="image-one" Type="{REL}/image" Target="media/pixel.png"/>'
        f'<Relationship Id="image-two" Type="{REL}/image" Target="media/pixel.png"/>'
    )
    package(
        "duplicate-content-assets.docx",
        document(
            duplicate_body,
            'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"',
        ),
        {
            "word/one.html": ("text/html", b"<p>same nested text</p>"),
            "word/two.html": ("text/html", b"<p>same nested text</p>"),
            "word/media/pixel.png": ("image/png", PNG),
        },
        duplicate_rels,
    )
    scenarios = {
        "html.docx": "internal HTML altChunk in body order",
        "xhtml.docx": "internal XHTML altChunk",
        "mhtml.docx": "bounded multipart MHTML altChunk",
        "rtf.docx": "local RTF altChunk",
        "strict.docx": "Strict WordprocessingML and relationship QNames",
        "external.docx": "external altChunk must never be fetched",
        "cycle.docx": "altChunk relationship cycle must not recurse",
        "entity.docx": "DTD and external entity must fail closed",
        "depth-limit.docx": "default XML nesting hard limit",
        "empty.docx": "verifiably empty Word body",
        "wrappers.docx": "content controls, compatibility fallback, fields, text boxes, headers and footers",
        "ordered-nested-merged.docx": "body/table/nested-table/merge/altChunk order",
        "duplicate-content-assets.docx": "repeated text and equal nested payloads remain ordered",
    }
    manifest = {
        "schemaVersion": 1,
        "generator": "generate.py",
        "policyMatrix": ["bestEffort", "strict"],
        "fixtures": [
            {
                "path": name,
                "bytes": (ROOT / name).stat().st_size,
                "sha256": hashlib.sha256((ROOT / name).read_bytes()).hexdigest(),
                "scenario": scenario,
            }
            for name, scenario in sorted(scenarios.items())
        ],
    }
    (ROOT / "manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        newline="\n",
    )


if __name__ == "__main__":
    main()
