#!/usr/bin/env python3
"""Generate the repository-owned fixture corpus deterministically."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import struct
import sys
import tempfile
import unicodedata
import zipfile
from pathlib import Path

GENERATOR_VERSION = "1.0.0"
GENERATOR_SEED = 20260813
FIXED_ZIP_TIME = (2026, 1, 1, 0, 0, 0)
FONT_SHA256 = "2c76254f6fc379fddfce0a7e84fb5385bb135d3e399294f6eeb6680d0365b74b"
FONT_URL = (
    "https://raw.githubusercontent.com/notofonts/noto-cjk/"
    "f8d157532fbfaeda587e826d4cd5b21a49186f7c/"
    "Sans/OTF/SimplifiedChinese/NotoSansCJKsc-Regular.otf"
)
MODEL_URL = (
    "https://paddle-model-ecology.bj.bcebos.com/paddlex/official_inference_model/"
    "paddle3.0.0/PP-OCRv6_tiny_rec_onnx_infer.tar"
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def write(root: Path, relative: str, data: bytes) -> dict[str, object]:
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return {"path": relative, "bytes": len(data), "sha256": sha256(data)}


def zip_bytes(entries: list[tuple[str, bytes]]) -> bytes:
    import io

    output = io.BytesIO()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_STORED) as archive:
        for name, data in entries:
            info = zipfile.ZipInfo(name, FIXED_ZIP_TIME)
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            info.compress_type = zipfile.ZIP_STORED
            archive.writestr(info, data)
    return output.getvalue()


def docx(document_xml: bytes, relationships: bytes | None = None) -> bytes:
    entries = [
        (
            "[Content_Types].xml",
            b'<?xml version="1.0" encoding="UTF-8"?>'
            b'<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">'
            b'<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>'
            b'<Default Extension="xml" ContentType="application/xml"/>'
            b'<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>'
            b'</Types>',
        ),
        (
            "_rels/.rels",
            b'<?xml version="1.0" encoding="UTF-8"?>'
            b'<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
            b'<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>'
            b'</Relationships>',
        ),
        ("word/document.xml", document_xml),
    ]
    if relationships is not None:
        entries.append(("word/_rels/document.xml.rels", relationships))
    return zip_bytes(entries)


def epub3(chapter_xhtml: bytes) -> bytes:
    return zip_bytes(
        [
            ("mimetype", b"application/epub+zip"),
            (
                "META-INF/container.xml",
                b'<?xml version="1.0"?>'
                b'<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container" version="1.0">'
                b'<rootfiles><rootfile full-path="EPUB/package.opf" media-type="application/oebps-package+xml"/>'
                b'</rootfiles></container>',
            ),
            (
                "EPUB/package.opf",
                b'<?xml version="1.0"?>'
                b'<package xmlns="http://www.idpf.org/2007/opf" version="3.3" unique-identifier="uid">'
                b'<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">'
                b'<dc:identifier id="uid">urn:uuid:repository-epub-corpus</dc:identifier>'
                b'<dc:title>Corpus EPUB</dc:title><dc:language>en</dc:language>'
                b'<meta property="dcterms:modified">2026-08-13T00:00:00Z</meta>'
                b'</metadata><manifest>'
                b'<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>'
                b'<item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>'
                b'</manifest><spine><itemref idref="chapter"/></spine></package>',
            ),
            (
                "EPUB/nav.xhtml",
                b'<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">'
                b'<head><title>Contents</title></head><body><nav epub:type="toc"><ol>'
                b'<li><a href="chapter.xhtml#corpus">Corpus chapter</a></li>'
                b'</ol></nav></body></html>',
            ),
            ("EPUB/chapter.xhtml", chapter_xhtml),
        ]
    )


CFB_END = 0xFFFFFFFE
CFB_FREE = 0xFFFFFFFF
CFB_FAT = 0xFFFFFFFD


def mapi_variable(property_id: int, property_type: int, data: bytes) -> tuple[int, int, bytes, bytes]:
    return property_id, property_type, struct.pack("<I", len(data)) + b"\0" * 4, data


def mapi_unicode(property_id: int, value: str) -> tuple[int, int, bytes, bytes]:
    data = value.encode("utf-16le")
    return property_id, 0x001F, struct.pack("<I", len(data) + 2) + b"\0" * 4, data


def mapi_binary(property_id: int, value: bytes) -> tuple[int, int, bytes, bytes]:
    return mapi_variable(property_id, 0x0102, value)


def mapi_long(property_id: int, value: int) -> tuple[int, int, bytes, None]:
    return property_id, 0x0003, struct.pack("<i", value) + b"\0" * 4, None


def mapi_time(property_id: int, value: int) -> tuple[int, int, bytes, None]:
    return property_id, 0x0040, struct.pack("<Q", value), None


def mapi_object(property_id: int) -> tuple[int, int, bytes, None]:
    return property_id, 0x000D, struct.pack("<II", 0xFFFFFFFF, 1), None


def mapi_properties(records: list[tuple[int, int, bytes, bytes | None]], root: bool, recipients: int = 0, attachments: int = 0) -> bytes:
    header = bytearray(32 if root else 8)
    if root:
        struct.pack_into("<II", header, 16, recipients, attachments)
    output = bytearray(header)
    for property_id, property_type, value, _ in records:
        output.extend(struct.pack("<II", property_id << 16 | property_type, 0))
        output.extend(value)
    return bytes(output)


def add_mapi_storage(
    entries: list[tuple[tuple[str, ...], bytes | None]],
    base: tuple[str, ...],
    records: list[tuple[int, int, bytes, bytes | None]],
    root: bool,
    recipients: int = 0,
    attachments: int = 0,
) -> None:
    if base:
        entries.append((base, None))
    for property_id, property_type, _, stream in records:
        if stream is not None:
            entries.append((base + (f"__substg1.0_{property_id:04X}{property_type:04X}",), stream))
    entries.append((base + ("__properties_version1.0",), mapi_properties(records, root, recipients, attachments)))


def cfb(entries: list[tuple[tuple[str, ...], bytes | None]]) -> bytes:
    paths: dict[tuple[str, ...], bytes | None] = {(): None}
    for path, data in entries:
        for length in range(1, len(path)):
            paths.setdefault(path[:length], None)
        if path in paths:
            raise ValueError(f"duplicate CFB path: {path}")
        paths[path] = data
    directory = [
        {"path": path, "data": data, "left": CFB_FREE, "right": CFB_FREE, "child": CFB_FREE, "start": CFB_END}
        for path, data in sorted(paths.items(), key=lambda item: (len(item[0]), item[0]))
    ]
    for parent, entry in enumerate(directory):
        if entry["data"] is not None:
            continue
        children = [
            index for index, candidate in enumerate(directory)
            if len(candidate["path"]) == len(entry["path"]) + 1 and candidate["path"][:-1] == entry["path"]
        ]
        children.sort(key=lambda index: directory[index]["path"][-1])
        if children:
            directory[parent]["child"] = children[0]
        for left, right in zip(children, children[1:]):
            directory[left]["right"] = right

    mini_data = bytearray()
    minifat: list[int] = []
    for entry in directory:
        data = entry["data"]
        if data is None or not data:
            continue
        entry["start"] = len(minifat)
        count = (len(data) + 63) // 64
        for offset in range(count):
            minifat.append(CFB_END if offset + 1 == count else len(minifat) + 1)
        mini_data.extend(data)
        mini_data.extend(b"\0" * (-len(mini_data) % 64))

    directory_sectors = (len(directory) * 128 + 511) // 512
    minifat_sectors = (len(minifat) * 4 + 511) // 512
    root_sectors = (len(mini_data) + 511) // 512
    minifat_start = directory_sectors
    root_start = minifat_start + minifat_sectors
    fat_sector = root_start + root_sectors
    directory[0]["start"] = root_start if root_sectors else CFB_END

    directory_bytes = bytearray()
    for index, entry in enumerate(directory):
        raw = bytearray(128)
        name = "Root Entry" if index == 0 else entry["path"][-1]
        encoded = name.encode("utf-16le") + b"\0\0"
        if len(encoded) > 64:
            raise ValueError(f"CFB name is too long: {name}")
        raw[: len(encoded)] = encoded
        struct.pack_into("<HBBIII", raw, 64, len(encoded), 5 if index == 0 else (2 if entry["data"] is not None else 1), 1, entry["left"], entry["right"], entry["child"])
        size = len(mini_data) if index == 0 else len(entry["data"] or b"")
        struct.pack_into("<IQ", raw, 116, entry["start"], size)
        directory_bytes.extend(raw)
    directory_bytes.extend(b"\0" * (directory_sectors * 512 - len(directory_bytes)))
    minifat_bytes = bytearray().join(struct.pack("<I", value) for value in minifat)
    minifat_bytes.extend(b"\xff" * (minifat_sectors * 512 - len(minifat_bytes)))
    mini_data.extend(b"\0" * (root_sectors * 512 - len(mini_data)))

    fat_entries = [CFB_FREE] * 128
    def chain(start: int, count: int) -> None:
        for offset in range(count):
            fat_entries[start + offset] = CFB_END if offset + 1 == count else start + offset + 1
    chain(0, directory_sectors)
    chain(minifat_start, minifat_sectors)
    chain(root_start, root_sectors)
    fat_entries[fat_sector] = CFB_FAT
    header = bytearray(512)
    header[:8] = bytes.fromhex("d0cf11e0a1b11ae1")
    struct.pack_into("<HHHH", header, 24, 0x003E, 3, 0xFFFE, 9)
    struct.pack_into("<H", header, 32, 6)
    struct.pack_into("<II", header, 44, 1, 0)
    struct.pack_into("<I", header, 56, 4096)
    struct.pack_into("<II", header, 60, minifat_start if minifat_sectors else CFB_END, minifat_sectors)
    struct.pack_into("<II", header, 68, CFB_END, 0)
    struct.pack_into("<I", header, 76, fat_sector)
    for offset in range(80, 512, 4):
        struct.pack_into("<I", header, offset, CFB_FREE)
    fat_bytes = b"".join(struct.pack("<I", value) for value in fat_entries)
    return bytes(header + directory_bytes + minifat_bytes + mini_data + fat_bytes)


def lzfu_uncompressed(raw: bytes) -> bytes:
    return struct.pack("<IIII", len(raw) + 12, len(raw), 0x414C454D, 0) + raw


def embedded_msg_entries(body: str) -> list[tuple[tuple[str, ...], bytes | None]]:
    entries: list[tuple[tuple[str, ...], bytes | None]] = []
    add_mapi_storage(entries, (), [mapi_unicode(0x0037, "Nested fixture"), mapi_unicode(0x1000, body)], True)
    return entries


def outlook_msg(
    body: str | None = None,
    html: bytes | None = None,
    rtf: bytes | None = None,
    cid: bool = False,
    attachment: bool = False,
    nested: bool = False,
) -> bytes:
    entries: list[tuple[tuple[str, ...], bytes | None]] = []
    root = [
        mapi_unicode(0x0037, "Repository MSG"),
        mapi_unicode(0x0C1A, "Alice"),
        mapi_unicode(0x0C1F, "alice@example.test"),
        mapi_long(0x3FFD, 1252),
        mapi_time(0x0039, 116444736000000000),
        mapi_unicode(0x007D, "Message-ID: <repository@example.test>\r\nX-Offline: true\r\n"),
    ]
    if body is not None:
        root.append(mapi_unicode(0x1000, body))
    if html is not None:
        root.append(mapi_binary(0x1013, html))
    if rtf is not None:
        root.append(mapi_binary(0x1009, rtf))
    attachment_count = int(cid) + int(attachment) + int(nested)
    add_mapi_storage(entries, (), root, True, recipients=1, attachments=attachment_count)
    add_mapi_storage(
        entries,
        ("__recip_version1.0_#00000000",),
        [mapi_long(0x0C15, 1), mapi_unicode(0x3001, "Bob"), mapi_unicode(0x39FE, "bob@example.test")],
        False,
    )
    attachment_index = 0
    if cid:
        base = (f"__attach_version1.0_#{attachment_index:08X}",)
        add_mapi_storage(entries, base, [
            mapi_long(0x3705, 1), mapi_unicode(0x3707, "logo.png"),
            mapi_unicode(0x370E, "image/png"), mapi_unicode(0x3712, "logo@example.test"),
            mapi_binary(0x3701, bytes.fromhex("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d49444154789c6360606060000000050001a5f645400000000049454e44ae426082")),
        ], False)
        attachment_index += 1
    if attachment:
        base = (f"__attach_version1.0_#{attachment_index:08X}",)
        add_mapi_storage(entries, base, [
            mapi_long(0x3705, 1), mapi_unicode(0x3707, "notes.txt"),
            mapi_unicode(0x370E, "text/plain"), mapi_binary(0x3701, b"repository attachment"),
        ], False)
        attachment_index += 1
    if nested:
        base = (f"__attach_version1.0_#{attachment_index:08X}",)
        add_mapi_storage(entries, base, [
            mapi_long(0x3705, 5), mapi_unicode(0x3707, "forwarded.msg"), mapi_object(0x3701),
        ], False)
        object_base = base + ("__substg1.0_3701000D",)
        entries.append((object_base, None))
        for path, data in embedded_msg_entries("Nested fixture body"):
            entries.append((object_base + path, data))
    return cfb(entries)


def msg_fixture_definitions() -> list[tuple[str, str, bytes, dict[str, object]]]:
    plain = outlook_msg(body="Plain fixture body")
    html = outlook_msg(body="fallback", html=b"<main><h2>HTML fixture</h2><p>semantic body</p></main>")
    cid = outlook_msg(html=b"<main><p>CID fixture</p><img src='cid:logo@example.test' alt='logo'></main>", cid=True)
    nested = outlook_msg(body="Outer fixture body", attachment=True, nested=True)
    rtf = outlook_msg(rtf=lzfu_uncompressed(b"{\\rtf1\\ansi Repository RTF body}"))
    malicious = bytearray(plain)
    fat_sector = struct.unpack_from("<I", malicious, 76)[0]
    struct.pack_into("<I", malicious, (fat_sector + 1) * 512, 0)
    return [
        ("msg-normal", "normal", plain, expected_hash("headers, time, transport headers and plain body", "747acafb3f0a1bd58024b276273edf1ade3cfb83ace9ca42ce54423f0a171ea3")),
        ("msg-html", "html", html, expected_hash("HTML is selected ahead of the plain fallback", "60837228af31fb2a540e6128aabbe3dd3678a1faf57d35e19d9eff7a7e2ba3a1")),
        ("msg-cid", "cid", cid, expected_hash("canonical CID image is bound at its HTML reference", "9f4f89545f5fbf6f45e348ed69d4b26410516310f8736cb632b890aefc1112af")),
        ("msg-attachment-nested", "attachment", nested, expected_hash("by-value and embedded MSG attachments retain assets and source chains", "8cb76c2394e297ccf0845ed118f745ce1c1c1a79b5032976a128914e5acf9b3a")),
        ("msg-rtf", "rtf", rtf, expected_hash("LZFu is decoded and passed to the bounded RTF converter on the same request context", "9a9f5eaf2525c8669f22eb27199535b3f6eb6cd5415c0835d30593a398103367")),
        ("msg-corrupt", "corrupt", plain[:-17], expected("error", "truncated CFB sector", error_code="malformed")),
        ("msg-malicious", "malicious", bytes(malicious), expected("error", "cyclic CFB directory FAT chain", error_code="malformed")),
        ("msg-limit", "limit", plain, limit_expected("MSG crosses the exact input byte boundary", "max_input_bytes", len(plain) - 1, len(plain), "max_input_bytes", "", "747acafb3f0a1bd58024b276273edf1ade3cfb83ace9ca42ce54423f0a171ea3")),
    ]


def expected(
    outcome: str,
    description: str,
    semantic: str = "",
    error_code: str = "",
    limit: dict[str, object] | None = None,
) -> dict[str, object]:
    result: dict[str, object] = {
        "outcome": outcome,
        "error_code": error_code,
        "semantic_sha256": sha256(semantic.encode("utf-8")) if semantic else "",
        "description": description,
    }
    if limit is not None:
        result["limit"] = limit
    return result


def expected_hash(description: str, semantic_sha256: str) -> dict[str, object]:
    return {
        "outcome": "success",
        "error_code": "",
        "semantic_sha256": semantic_sha256,
        "description": description,
    }


def limit_expected(
    description: str,
    option: str,
    failing_value: int,
    passing_value: int,
    error_limit: str,
    passing_semantic: str,
    passing_semantic_sha256: str = "",
) -> dict[str, object]:
    return expected(
        "error",
        description,
        error_code="resourceLimit",
        limit={
            "option": option,
            "failing_value": failing_value,
            "passing_value": passing_value,
            "error_limit": error_limit,
            "passing_semantic_sha256": passing_semantic_sha256 or sha256(passing_semantic.encode("utf-8")),
        },
    )


def generated_fixture(
    root: Path,
    fixture_id: str,
    format_name: str,
    scenario: str,
    relative: str,
    data: bytes,
    media_type: str,
    result: dict[str, object],
) -> dict[str, object]:
    file_info = write(root, relative, data)
    return {
        "id": fixture_id,
        "format": format_name,
        "scenario": scenario,
        **file_info,
        "media_type": media_type,
        "license": {
            "spdx": "Apache-2.0",
            "copyright": "2026 into-markdown contributors",
            "redistribution": "repository and release test artifacts permitted",
        },
        "provenance": {
            "kind": "repository-generated",
            "source_url": "",
            "author": "into-markdown contributors",
            "acquired_on": "2026-08-13",
            "generator": "fixtures/generate.py@1.0.0",
            "source_sha256": "",
        },
        "expected": result,
    }


def write_msg_fixtures(root: Path) -> list[dict[str, object]]:
    return [
        generated_fixture(
            root,
            fixture_id,
            "outlook-msg",
            scenario,
            f"small/msg/{fixture_id.removeprefix('msg-')}.msg",
            data,
            "application/vnd.ms-outlook",
            result,
        )
        for fixture_id, scenario, data, result in msg_fixture_definitions()
    ]


def patch_encrypted_flag(archive: bytes) -> bytes:
    data = bytearray(archive)
    offset = 0
    patched = 0
    while offset + 4 <= len(data):
        signature = bytes(data[offset : offset + 4])
        if signature == b"PK\x03\x04":
            flags = struct.unpack_from("<H", data, offset + 6)[0] | 1
            struct.pack_into("<H", data, offset + 6, flags)
            name_len, extra_len = struct.unpack_from("<HH", data, offset + 26)
            compressed = struct.unpack_from("<I", data, offset + 18)[0]
            offset += 30 + name_len + extra_len + compressed
            patched += 1
        elif signature == b"PK\x01\x02":
            flags = struct.unpack_from("<H", data, offset + 8)[0] | 1
            struct.pack_into("<H", data, offset + 8, flags)
            name_len, extra_len, comment_len = struct.unpack_from("<HHH", data, offset + 28)
            offset += 46 + name_len + extra_len + comment_len
        elif signature == b"PK\x05\x06":
            break
        else:
            raise ValueError("unexpected ZIP record while setting encryption flag")
    if patched == 0:
        raise ValueError("ZIP contains no local entries")
    return bytes(data)


def render_ocr(root: Path, font_path: Path) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
    if sha256(font_path.read_bytes()) != FONT_SHA256:
        raise SystemExit("font SHA-256 does not match the fixture authority")
    try:
        import PIL
        from PIL import Image, ImageDraw, ImageFont, features
    except ImportError as error:
        raise SystemExit("Pillow 11.3.0 is required for OCR fixture regeneration") from error
    if PIL.__version__ != "11.3.0" or features.version_module("freetype2") != "2.13.3":
        raise SystemExit("OCR fixture regeneration requires Pillow 11.3.0 with FreeType 2.13.3")
    if sys.version_info[:3] != (3, 13, 14):
        raise SystemExit("OCR fixture regeneration requires CPython 3.13.14")

    texts = [
        ("ocr-simplified-clear-1", "simplified", "清晰扫描文本用于验证文档转换质量与字符顺序。", 0.05),
        ("ocr-simplified-clear-2", "simplified", "离线处理应保持标题、段落、数字二零二六和标点。", 0.05),
        ("ocr-simplified-clear-3", "simplified", "安全边界拒绝损坏输入并返回稳定诊断结果。", 0.05),
        ("ocr-traditional-clear-1", "traditional", "清晰掃描文字用於驗證文件轉換品質與字元順序。", 0.10),
        ("ocr-traditional-clear-2", "traditional", "離線處理應保持標題、段落、數字二零二六和標點。", 0.10),
        ("ocr-traditional-clear-3", "traditional", "安全邊界拒絕損壞輸入並傳回穩定診斷結果。", 0.10),
        ("ocr-english-clear-1", "english", "Clear scans verify document conversion quality and character order.", 0.05),
        ("ocr-english-clear-2", "english", "Offline processing keeps headings, paragraphs, digits 2026, and punctuation.", 0.05),
        ("ocr-english-clear-3", "english", "Safety limits reject damaged input with stable diagnostic results.", 0.05),
        ("ocr-mixed-clear-1", "mixed", "中英混排 OCR Quality 验证文件轉換與 order 2026。", 0.08),
        ("ocr-mixed-clear-2", "mixed", "离线 Pipeline 保持標題、paragraphs、数字 42 和 punctuation。", 0.08),
        ("ocr-mixed-clear-3", "mixed", "安全 boundary 拒绝 damaged 輸入並返回 stable diagnostics。", 0.08),
    ]
    fixtures: list[dict[str, object]] = []
    goldens: list[dict[str, object]] = []
    font = ImageFont.truetype(str(font_path), 42, layout_engine=ImageFont.Layout.BASIC)
    for fixture_id, group, text, threshold in texts:
        assert unicodedata.normalize("NFC", text) == text
        image = Image.new("L", (1800, 80), 255)
        draw = ImageDraw.Draw(image)
        draw.text((20, 9), text, font=font, fill=0, stroke_width=0)
        import io

        encoded = io.BytesIO()
        image.save(encoded, "PNG", optimize=False, compress_level=9)
        relative = f"small/ocr/{fixture_id}.png"
        fixtures.append(
            generated_fixture(
                root,
                fixture_id,
                "ocr-image",
                "normal",
                relative,
                encoded.getvalue(),
                "image/png",
                expected("success", f"clear {group} OCR line", text),
            )
        )
        fixture_sha256 = str(fixtures[-1]["sha256"])
        semantic_sha256 = str(fixtures[-1]["expected"]["semantic_sha256"])
        if semantic_sha256 != sha256(text.encode("utf-8")):
            raise AssertionError("OCR fixture semantic hash is not bound to its golden text")
        evaluated = "".join(character for character in text if not character.isspace())
        goldens.append(
            {
                "fixture_id": fixture_id,
                "fixture_sha256": fixture_sha256,
                "group": group,
                "ground_truth_nfc": text,
                "codepoints": len(text),
                "evaluated_characters": len(evaluated),
                "maximum_cer": threshold,
            }
        )
    return fixtures, goldens


def build(root: Path, font_path: Path) -> None:
    generated_root = root / "small"
    if generated_root.is_dir():
        shutil.rmtree(generated_root)
    fixtures: list[dict[str, object]] = []
    add = lambda *args: fixtures.append(generated_fixture(root, *args))

    add("text-normal", "text", "normal", "small/text/normal.txt", "Alpha 中文 line\nSecond line\n".encode(), "text/plain", expected("success", "two UTF-8 paragraphs", "Alpha 中文 line  \nSecond line\n"))
    add("text-corrupt", "text", "corrupt", "small/text/corrupt.txt", b"valid\n\xff\xfe\x00tail", "text/plain", expected("error", "invalid UTF-8/UTF-16 byte sequence", error_code="malformed"))
    text_limit = b"x" * 4096
    add("text-limit", "text", "limit", "small/text/limit.txt", text_limit, "text/plain", limit_expected("input exceeds the exact configured byte budget", "max_input_bytes", len(text_limit) - 1, len(text_limit), "max_input_bytes", "", "5d05023e33a88151e829770ac45c53a2acd6f5bd111cbb101b8d649d7f9c2906"))

    add("markdown-normal", "markdown", "normal", "small/markdown/normal.md", b"# Corpus\n\n- alpha\n- **beta**\n", "text/markdown", expected("success", "heading and rich list", "# Corpus\n\n- alpha\n- <strong>beta</strong>\n"))
    add("markdown-corrupt", "markdown", "corrupt", "small/markdown/corrupt.md", b"# valid\n\xffbroken", "text/markdown", expected("error", "invalid UTF-8", error_code="malformed"))
    markdown_limit = (("> " * 8) + "deep\n").encode()
    add("markdown-limit", "markdown", "limit", "small/markdown/limit.md", markdown_limit, "text/markdown", limit_expected("nested block quote crosses the exact parser-depth boundary", "max_nesting_depth", 8, 9, "max_nesting_depth", "", "0b5587bf924fa4b9abd8f28f256f553079f3cd64b8493f5b059c4e238cf7a76b"))

    add("html-normal", "html", "normal", "small/html/normal.html", "<!doctype html><html lang=en><body><main><h1>Corpus</h1><p>Alpha 中文</p></main></body></html>".encode(), "text/html", expected("success", "main landmark with heading and paragraph", "# Corpus\n\nAlpha 中文\n"))
    add("html-corrupt", "html", "corrupt", "small/html/corrupt.html", b"<html><body>\xff</body></html>", "text/html", expected("error", "invalid explicit UTF-8", error_code="malformed"))
    html_limit = (("<div>" * 8) + "deep" + ("</div>" * 8)).encode()
    add("html-limit", "html", "limit", "small/html/limit.html", html_limit, "text/html", limit_expected("DOM crosses the exact configured depth boundary", "max_nesting_depth", 10, 11, "html_nesting_depth", "", "64896f89fd11190013b70103e603a1c5826e56b7fb7d2197ab279b0690043599"))
    add("html-malicious", "html", "malicious", "small/html/malicious.html", b"<!doctype html><main><script>secret()</script><a href='javascript:alert(1)'>unsafe</a><p>safe</p></main>", "text/html", expected("success", "active content and unsafe URL are omitted", "unsafe\n\nsafe\n"))

    feed_normal = b'<rss version="2.0"><channel><title>Corpus Feed</title><item><guid>entry-1</guid><title>Alpha</title><description>Safe text</description></item></channel></rss>\n'
    add("feed-normal", "feed", "normal", "small/feed/normal.xml", feed_normal, "application/rss+xml", expected("success", "RSS item title and safe description", "## Alpha\n\n### Summary\n\nSafe text\n"))
    add("feed-corrupt", "feed", "corrupt", "small/feed/corrupt.xml", b'<!DOCTYPE rss [<!ENTITY secret "payload">]><rss version="2.0"><channel><title>&secret;</title></channel></rss>\n', "application/rss+xml", expected("error", "DTD and custom entity are rejected", error_code="malformed"))
    feed_limit = b'<rss version="2.0"><channel><title>Boundary Feed</title><item><guid>entry-limit</guid><title>Boundary</title><description>Exact input byte boundary</description></item></channel></rss>\n'
    add("feed-limit", "feed", "limit", "small/feed/limit.xml", feed_limit, "application/rss+xml", limit_expected("feed input exceeds the exact configured byte budget", "max_input_bytes", len(feed_limit) - 1, len(feed_limit), "max_input_bytes", "", "f6d6668e4b78299ae7b951b186b9c338b0c935f8aa4423122ba3199e77dfac06"))

    add("csv-normal", "csv", "normal", "small/csv/normal.csv", "name,value\nalpha,1\n中文,2\n".encode(), "text/csv", expected("success", "header and two rows", "| <strong>name</strong> | <strong>value</strong> |\n| --- | --- |\n| alpha | 1 |\n| 中文 | 2 |\n"))
    add("csv-corrupt", "csv", "corrupt", "small/csv/corrupt.csv", b'name,value\n"unterminated,1\n', "text/csv", expected("error", "unterminated quoted field", error_code="malformed"))
    csv_limit = ((",".join(f"c{i}" for i in range(33))) + "\n").encode()
    add("csv-limit", "csv", "limit", "small/csv/limit.csv", csv_limit, "text/csv", limit_expected("column count crosses the exact configured boundary", "max_table_columns", 32, 33, "max_table_columns", "", "bedc28a22c18681a165e567d8bb7c818230e6ff7dc047b8484ed229ba4db3d5e"))

    add("tsv-normal", "tsv", "normal", "small/tsv/normal.tsv", "name\tvalue\nalpha\t1\n繁體\t2\n".encode(), "text/tab-separated-values", expected("success", "header and two rows", "| <strong>name</strong> | <strong>value</strong> |\n| --- | --- |\n| alpha | 1 |\n| 繁體 | 2 |\n"))
    add("tsv-corrupt", "tsv", "corrupt", "small/tsv/corrupt.tsv", b"name\tvalue\nvalid\t\xff\n", "text/tab-separated-values", expected("error", "invalid UTF-8", error_code="malformed"))
    tsv_limit = (("\t".join(f"c{i}" for i in range(33))) + "\n").encode()
    add("tsv-limit", "tsv", "limit", "small/tsv/limit.tsv", tsv_limit, "text/tab-separated-values", limit_expected("column count crosses the exact configured boundary", "max_table_columns", 32, 33, "max_table_columns", "", "bedc28a22c18681a165e567d8bb7c818230e6ff7dc047b8484ed229ba4db3d5e"))

    add("json-normal", "json", "normal", "small/json/normal.json", b'{"title":"Corpus","items":[1,2,3]}\n', "application/json", expected("success", "object with ordered array", "# JSON\n\n`title`: `\"Corpus\"`\n\n### items\n\n`[0]`: `1`\n\n`[1]`: `2`\n\n`[2]`: `3`\n"))
    add("json-corrupt", "json", "corrupt", "small/json/corrupt.json", b'{"title":"Corpus",}\n', "application/json", expected("error", "trailing comma", error_code="malformed"))
    json_limit = (("[" * 8) + "0" + ("]" * 8)).encode()
    add("json-limit", "json", "limit", "small/json/limit.json", json_limit, "application/json", limit_expected("JSON crosses the exact configured nesting boundary", "max_nesting_depth", 7, 8, "json_nesting_depth", "", "bd5491d5b6fcc8aab48d9c8b2b9a251e31dfaffd603aff7023845b1aa611d8e1"))

    add("xml-normal", "xml", "normal", "small/xml/normal.xml", "<?xml version='1.0'?><document><title>Corpus</title><p>Alpha 中文</p></document>".encode(), "application/xml", expected("success", "title and paragraph elements", "# document (local=document, prefix=\"\", namespace=\"\")\n\n## title (local=title, prefix=\"\", namespace=\"\")\n\nCorpus\n\n## p (local=p, prefix=\"\", namespace=\"\")\n\nAlpha 中文\n"))
    add("xml-corrupt", "xml", "corrupt", "small/xml/corrupt.xml", b"<?xml version='1.0'?><document><p>broken</document>", "application/xml", expected("error", "mismatched closing element", error_code="malformed"))
    xml_limit = (("<n>" * 8) + "x" + ("</n>" * 8)).encode()
    add("xml-limit", "xml", "limit", "small/xml/limit.xml", xml_limit, "application/xml", limit_expected("XML crosses the exact configured nesting boundary", "max_nesting_depth", 7, 8, "xml_nesting_depth", "", "466e6540a9adaabdbd98b8a065097b4b4ddcc3b48edd4fae25636d88d08e3d55"))
    add("xml-malicious", "xml", "malicious", "small/xml/malicious.xml", b'<!DOCTYPE x [<!ENTITY e "payload">]><x>&e;</x>', "application/xml", expected("error", "DTD and custom entity are rejected", error_code="malformed"))

    notebook = b'{"cells":[{"id":"normal","cell_type":"markdown","metadata":{},"source":["# Corpus\\n","Alpha"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}\n'
    add("ipynb-normal", "ipynb", "normal", "small/ipynb/normal.ipynb", notebook, "application/x-ipynb+json", expected("success", "one markdown cell", "# Corpus\n\nAlpha\n"))
    add("ipynb-corrupt", "ipynb", "corrupt", "small/ipynb/corrupt.ipynb", b'{"cells":[}\n', "application/x-ipynb+json", expected("error", "invalid notebook JSON", error_code="malformed"))
    ipynb_limit = b'{"cells":[],"metadata":{"a":{"b":{"c":{"d":{}}}}},"nbformat":4,"nbformat_minor":5}'
    add("ipynb-limit", "ipynb", "limit", "small/ipynb/limit.ipynb", ipynb_limit, "application/x-ipynb+json", limit_expected("notebook JSON crosses the exact configured nesting boundary", "max_nesting_depth", 4, 5, "json_nesting_depth", ""))
    add("ipynb-malicious", "ipynb", "malicious", "small/ipynb/malicious.ipynb", b'{"cells":[{"id":"active-output","cell_type":"code","execution_count":1,"metadata":{},"outputs":[{"output_type":"display_data","data":{"text/html":["<script>secret()</script><p>safe</p>"]},"metadata":{}}],"source":["print(1)"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}', "application/x-ipynb+json", expected("success", "active HTML output is inert inside a fenced code block", "### Code cell \\[1\\]\n\n```\nprint(1)\n```\n\n```html\n<script>secret()</script><p>safe</p>\n```\n"))

    normal_document = b'<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Corpus Alpha \xe4\xb8\xad\xe6\x96\x87</w:t></w:r></w:p></w:body></w:document>'
    normal_docx = docx(normal_document)
    add("docx-normal", "docx", "normal", "small/docx/normal.docx", normal_docx, "application/vnd.openxmlformats-officedocument.wordprocessingml.document", expected("success", "one WordprocessingML paragraph", "Corpus Alpha 中文\n"))
    add("docx-corrupt", "docx", "corrupt", "small/docx/corrupt.docx", normal_docx[: len(normal_docx) // 2], "application/vnd.openxmlformats-officedocument.wordprocessingml.document", expected("error", "truncated ZIP package", error_code="malformed"))
    nested = b'<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>x</w:t></w:r></w:p></w:body></w:document>'
    add("docx-limit", "docx", "limit", "small/docx/limit.docx", docx(nested), "application/vnd.openxmlformats-officedocument.wordprocessingml.document", limit_expected("WordprocessingML crosses the exact configured depth boundary", "max_nesting_depth", 4, 5, "max_nesting_depth", "", "73cb3858a687a8494ca3323053016282f3dad39d42cf62ca4e79dda2aac7d9ac"))
    add("docx-encrypted", "docx", "encrypted", "small/docx/encrypted.docx", patch_encrypted_flag(normal_docx), "application/vnd.openxmlformats-officedocument.wordprocessingml.document", expected("error", "encrypted ZIP entries are rejected", error_code="encrypted"))
    external_document = b'<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p><w:hyperlink r:id="rId9"><w:r><w:t>safe external link</w:t></w:r></w:hyperlink></w:p></w:body></w:document>'
    external = b'<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.invalid/fixture-link" TargetMode="External"/></Relationships>'
    add("docx-malicious", "docx", "malicious", "small/docx/malicious.docx", docx(external_document, external), "application/vnd.openxmlformats-officedocument.wordprocessingml.document", expected("success", "referenced external hyperlink is rendered without any service or network request", "[safe external link](<https://example.invalid/fixture-link>)\n"))

    rtf_normal = b"{\\rtf1\\ansi\\ansicpg1252 Corpus {\\b Alpha} \\u20013?\\u25991?\\par}\n"
    add("rtf-normal", "rtf", "normal", "small/rtf/normal.rtf", rtf_normal, "application/rtf", expected("success", "styled English and Unicode Chinese paragraph", "Corpus **Alpha** \u4e2d\u6587\n"))
    add("rtf-corrupt", "rtf", "corrupt", "small/rtf/corrupt.rtf", b"{\\rtf1\\ansi unterminated\n", "application/rtf", expected("error", "unterminated root group", error_code="malformed"))
    rtf_limit = ("{\\rtf1\\ansi " + ("{" * 8) + "deep" + ("}" * 8) + "}\n").encode()
    add("rtf-limit", "rtf", "limit", "small/rtf/limit.rtf", rtf_limit, "application/rtf", limit_expected("RTF group stack crosses the exact configured depth boundary", "max_nesting_depth", 8, 9, "max_nesting_depth", "deep\n"))
    rtf_malicious = b"{\\rtf1\\ansi before{\\object{\\*\\objdata 010203}{\\result hidden}}{\\field{\\*\\fldinst HYPERLINK \\\"file:///etc/passwd\\\"}{\\fldrslt unsafe}}after\\par}\n"
    add("rtf-malicious", "rtf", "malicious", "small/rtf/malicious.rtf", rtf_malicious, "application/rtf", expected("success", "embedded object and local-file hyperlink remain inert", "beforeunsafeafter\n"))
    fixtures.extend(write_msg_fixtures(root))

    epub_normal = epub3(
        b'<html xmlns="http://www.w3.org/1999/xhtml"><head><title>Corpus chapter</title></head>'
        b'<body><main><h1 id="corpus">Corpus chapter</h1><p>Alpha EPUB text.</p></main></body></html>'
    )
    add("epub-normal", "epub", "normal", "small/epub/normal.epub", epub_normal, "application/epub+zip", expected("success", "EPUB 3 package with navigation and one XHTML spine item", "# Contents\n\n1. [Corpus chapter](<EPUB/chapter.xhtml#corpus>)\n\n# Corpus chapter\n\n# Corpus chapter\n\nAlpha EPUB text\\.\n"))

    ocr_fixtures, ocr_goldens = render_ocr(root, font_path)
    fixtures.extend(ocr_fixtures)
    fixtures.sort(key=lambda item: str(item["id"]))

    manifest = {
        "schema_version": 1,
        "generator": {
            "path": "fixtures/generate.py",
            "version": GENERATOR_VERSION,
            "source_revision": "repository-commit",
            "sha256": sha256(Path(__file__).read_bytes()),
            "seed": GENERATOR_SEED,
            "python": "3.13.14",
            "pillow": "11.3.0",
            "freetype": "2.13.3",
            "reference_platform": "macos-11-arm64-cp313",
            "pillow_wheel_sha256": "7db51d222548ccfd274e4572fdbf3e810a5e66b00608862f947b163e613b67dd",
        },
        "available_formats": ["csv", "docx", "epub", "feed", "html", "ipynb", "json", "markdown", "outlook-msg", "pdf", "rtf", "text", "tsv", "xml", "zip"],
        "fixtures": fixtures,
        "large_artifacts": [
            {
                "id": "noto-sans-cjk-sc-regular-generator-font",
                "purpose": "OCR fixture generation only",
                "url": FONT_URL,
                "allowed_hosts": ["raw.githubusercontent.com"],
                "bytes": 16437364,
                "sha256": FONT_SHA256,
                "maximum_redirects": 0,
                "source_revision": "f8d157532fbfaeda587e826d4cd5b21a49186f7c",
                "source_container": "raw-file",
                "source_container_sha256": FONT_SHA256,
                "license": "OFL-1.1",
                "license_url": "https://raw.githubusercontent.com/notofonts/noto-cjk/f8d157532fbfaeda587e826d4cd5b21a49186f7c/Sans/LICENSE",
                "author": "Adobe and Google",
                "acquired_on": "2026-08-13",
                "redistribution": "font may be cached and redistributed under OFL-1.1; not included in repository or release",
                "manual_only": True,
                "included_in_release": False,
                "repository": "fixture_noto_cjk_font",
                "downloaded_file_path": "NotoSansCJKsc-Regular.otf",
            },
            {
                "id": "ppocrv6-tiny-recognizer-onnx-quality",
                "purpose": "explicit OCR quality target only",
                "url": MODEL_URL,
                "allowed_hosts": ["paddle-model-ecology.bj.bcebos.com"],
                "bytes": 4526080,
                "sha256": "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29",
                "maximum_redirects": 0,
                "source_revision": "PaddleOCR@2661c7c0ef5c613e8f93c6e93b2e052399f0f854",
                "source_container": "tar",
                "source_container_sha256": "1e13b22717b1edd89d4cde4fda272b6c17d5b505c97c2baea99da1a3a2d54b29",
                "license": "Apache-2.0",
                "license_url": "https://github.com/PaddlePaddle/PaddleOCR/blob/2661c7c0ef5c613e8f93c6e93b2e052399f0f854/LICENSE",
                "author": "PaddlePaddle Authors",
                "acquired_on": "2026-08-13",
                "redistribution": "cache and redistribution permitted with Apache-2.0 notices; not included in repository or release",
                "manual_only": True,
                "included_in_release": False,
                "repository": "fixture_ppocrv6_tiny_recognizer_onnx",
                "downloaded_file_path": "PP-OCRv6_tiny_rec_onnx_infer.tar",
            },
        ],
        "ocr_quality": {
            "license": "Apache-2.0",
            "copyright": "2026 into-markdown contributors",
            "unicode_normalization": "NFC",
            "line_endings": "LF",
            "whitespace_rule": "collapse Unicode whitespace runs, then exclude whitespace from edit distance",
            "punctuation_rule": "retain punctuation and compare Unicode scalar values exactly",
            "font_artifact_id": "noto-sans-cjk-sc-regular-generator-font",
            "render": {
                "mode": "L",
                "canvas_width": 1800,
                "canvas_height": 80,
                "font_size": 42,
                "origin_x": 20,
                "origin_y": 9,
                "foreground": 0,
                "background": 255,
                "png_compress_level": 9,
                "layout_engine": "Pillow ImageFont.Layout.BASIC",
                "antialiasing": "FreeType 2.13.3 grayscale rasterization",
                "locale": "locale-independent Unicode codepoint input",
                "dpi_metadata": "absent",
            },
            "training_pollution_statement": "Repository-authored phrases were created after the model release, are not copied from PaddleOCR training or evaluation data, and are used only as independent regression goldens.",
            "goldens": ocr_goldens,
        },
    }
    ocr_fixture_ids = {
        str(fixture["id"]) for fixture in fixtures if fixture["format"] == "ocr-image"
    }
    golden_ids = {str(golden["fixture_id"]) for golden in ocr_goldens}
    if ocr_fixture_ids != golden_ids or len(golden_ids) != len(ocr_goldens):
        raise AssertionError("OCR fixtures and goldens are not a one-to-one mapping")
    (root / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--font", type=Path)
    parser.add_argument("--output-root", type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument(
        "--msg-only",
        action="store_true",
        help="regenerate repository-authored MSG fixtures and update their manifest records",
    )
    parser.add_argument(
        "--verify",
        action="store_true",
        help="regenerate in a temporary directory and require byte equality with checked-in authority",
    )
    args = parser.parse_args()
    output_root = args.output_root.resolve()
    if args.msg_only:
        manifest_path = output_root / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["fixtures"] = [
            fixture
            for fixture in manifest["fixtures"]
            if fixture["format"] not in {"msg", "outlook-msg"}
        ] + write_msg_fixtures(output_root)
        manifest["fixtures"].sort(key=lambda item: str(item["id"]))
        manifest["available_formats"] = sorted(
            (set(manifest["available_formats"]) - {"msg"}) | {"outlook-msg"}
        )
        manifest["generator"]["sha256"] = sha256(Path(__file__).read_bytes())
        manifest_path.write_text(
            json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        return
    if args.font is None:
        parser.error("--font is required unless --msg-only is selected")
    if not args.verify:
        build(output_root, args.font.resolve())
        return
    with tempfile.TemporaryDirectory(prefix="into-markdown-fixtures-") as temporary:
        candidate = Path(temporary)
        build(candidate, args.font.resolve())
        expected = sorted(
            path.relative_to(output_root) for path in (output_root / "small").rglob("*") if path.is_file()
        ) + [Path("manifest.json")]
        actual = sorted(
            path.relative_to(candidate) for path in (candidate / "small").rglob("*") if path.is_file()
        ) + [Path("manifest.json")]
        if expected != actual:
            raise SystemExit("generated fixture path set differs from checked-in authority")
        for relative in expected:
            if (output_root / relative).read_bytes() != (candidate / relative).read_bytes():
                raise SystemExit(f"generated fixture differs: {relative.as_posix()}")


if __name__ == "__main__":
    main()
