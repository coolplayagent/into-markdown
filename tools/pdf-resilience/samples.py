"""Pinned, opt-in public PDF samples and a page-bounded derivative."""
from __future__ import annotations

import hashlib
import pathlib
import urllib.request

SAMPLES = {
    "Accenture_Humans_AI_Robots.pdf": (
        "https://www.accenture.com/content/dam/accenture/final/capabilities/strategy-and-consulting/strategy/document/Accenture-Humans-AI-Robots.pdf",
        1188879,
        "4c8b1e634ccc08987b32027539db1772a17cc19f585c78c6d59ed7a0395ef423",
    ),
    "CalculusVolume1-OP.pdf": (
        "https://assets.openstax.org/oscms-prodcms/media/documents/CalculusVolume1-OP.pdf",
        41375116,
        "202c86537285adf7e5abeb64057c39ee7333ad8c8473b6dd6a9ddf3e72443286",
    ),
}


def sha256(path: pathlib.Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def acquire(root: pathlib.Path) -> None:
    for name, (url, size, digest) in SAMPLES.items():
        target = root / name
        if not target.exists():
            # Fixed publisher URLs, explicit opt-in, bounded response and exact hash.
            with urllib.request.urlopen(url, timeout=60) as response:
                payload = response.read(size + 1)
            if len(payload) != size or hashlib.sha256(payload).hexdigest() != digest:
                raise ValueError(f"public sample authority changed: {name}")
            target.write_bytes(payload)
        if target.stat().st_size != size or sha256(target) != digest:
            raise ValueError(f"cached sample authority mismatch: {name}")


def excerpt(source: pathlib.Path, target: pathlib.Path, count: int = 600) -> dict:
    # add_page preserves annotations; explicitly remap internal destinations to
    # retained pages and omit only destinations outside the selected excerpt.
    from pypdf import PdfReader, PdfWriter
    from pypdf.generic import ArrayObject, IndirectObject, NameObject

    reader, writer = PdfReader(source), PdfWriter()
    indices = {p.indirect_reference.idnum: i for i, p in enumerate(reader.pages)}
    names = reader.named_destinations
    for page in reader.pages[:count]:
        writer.add_page(page)
    removed = 0
    retained = 0
    for old_page, new_page in zip(reader.pages[:count], writer.pages):
        old_annotations = old_page.get("/Annots", [])
        new_annotations = new_page.get("/Annots", [])
        assert len(old_annotations) == len(new_annotations), "annotation clone count changed"
        keep = ArrayObject()
        for old_ref, new_ref in zip(old_annotations, new_annotations):
            old, new = old_ref.get_object(), new_ref.get_object()
            action = old.get("/A", {})
            action = action.get_object() if hasattr(action, "get_object") else action
            dest = old.get("/Dest")
            action_dest = action.get("/S") == "/GoTo"
            if action_dest:
                dest = action.get("/D")
            if dest is not None:
                if isinstance(dest, str):
                    resolved = names.get(dest)
                    dest = resolved.dest_array if resolved is not None else None
                if dest is None or not isinstance(dest, ArrayObject):
                    raise ValueError("unresolved original PDF destination")
                reference = dest[0]
                index = indices.get(reference.idnum) if isinstance(reference, IndirectObject) else int(reference)
                if index is None or index >= count:
                    removed += 1
                    continue
                mapped = ArrayObject([writer.pages[index].indirect_reference, *(v.clone(writer) for v in dest[1:])])
                if action_dest:
                    new["/A"][NameObject("/D")] = mapped
                else:
                    new[NameObject("/Dest")] = mapped
            keep.append(new_ref)
            retained += 1
        if old_annotations:
            new_page[NameObject("/Annots")] = keep
    writer.write(target)
    return {"pages": count, "retainedAnnotations": retained, "omittedDestinationsOutsideExcerpt": removed, "sha256": sha256(target)}
