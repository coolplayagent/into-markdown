"""Generate complete LibreOffice runtime authority from the pinned DMG payload."""

from __future__ import annotations

import pathlib
import tempfile

from common import ReleaseError, regular_files, run, sha256, write_json

SYSTEM_PREFIXES = ("/System/Library/", "/usr/lib/")
LICENSE_NAMES = {"LICENSE", "LICENSE.html", "NOTICE", "CREDITS.fodt"}


def generate(
    runtime: pathlib.Path,
    worker: pathlib.Path,
    image: pathlib.Path,
    artifact: dict,
) -> pathlib.Path:
    target_worker = runtime / "legacy-office-worker"
    if target_worker != worker:
        target_worker.write_bytes(worker.read_bytes())
        target_worker.chmod(0o755)
    if image.stat().st_size != artifact["bytes"] or sha256(image) != artifact["sha256"]:
        raise ReleaseError("LibreOffice image disagrees with fixed authority")
    with tempfile.TemporaryDirectory(prefix="into-md-lo-authority-") as temporary:
        mount = pathlib.Path(temporary) / "mount"
        mount.mkdir()
        try:
            run([
                "/usr/bin/hdiutil", "attach", "-quiet", "-nobrowse", "-noautoopen",
                "-readonly", "-mountpoint", str(mount), str(image),
            ])
            app = mount / "LibreOffice.app"
            files = sorted(
                (path for path in app.rglob("*") if path.is_file()),
                key=lambda path: path.as_posix(),
            )
            kit = find_kit(files)
            system = dependency_closure(worker=target_worker, kit=kit, root=mount)
            kit_relative = kit.relative_to(mount).as_posix()
            kit_digest = sha256(kit)
            license_source = next(
                (path for path in files if path.name in LICENSE_NAMES), None
            )
            if license_source is None:
                raise ReleaseError("LibreOffice image omits reviewed license material")
            license_path = runtime / "LibreOffice-LICENSE.html"
            license_path.write_bytes(license_source.read_bytes())
        finally:
            run(["/usr/bin/hdiutil", "detach", "-quiet", str(mount)])
    inventory = []
    for path, role in [
        (target_worker, "worker"),
        (image, "runtime"),
        (license_path, "license"),
    ]:
        inventory.append({
            "path": path.relative_to(runtime).as_posix(),
            "bytes": path.stat().st_size,
            "sha256": sha256(path),
            "role": role,
        })
    license_digest = sha256(license_path)
    licenses = [{
        "id": "libreoffice-license-1",
        "spdx": None,
        "noticePath": license_path.relative_to(runtime).as_posix(),
        "noticeSha256": license_digest,
    }]
    value = {
        "schemaVersion": 1,
        "product": "LibreOffice",
        "version": "26.2.5",
        "sourceUrl": artifact["url"],
        "targets": {
            "aarch64-apple-darwin": {
                "artifactUrl": artifact["url"],
                "artifactBytes": artifact["bytes"],
                "artifactSha256": artifact["sha256"],
                "installRoot": "container/LibreOffice.app/Contents/Frameworks",
                "kitLibrary": f"container/{kit_relative}",
                "worker": "legacy-office-worker",
                "container": {
                    "format": "udif",
                    "imagePath": image.relative_to(runtime).as_posix(),
                    "imageBytes": artifact["bytes"],
                    "imageSha256": artifact["sha256"],
                    "mountPath": "container",
                    "kitSha256": kit_digest,
                },
                "files": inventory,
                "licenses": licenses,
                "abi": {
                    "binaryFormat": "mach-o",
                    "architecture": "aarch64",
                    "libraryIdentity": kit.name,
                    "requiredExport": "libreofficekit_hook_2",
                },
                "limits": {
                    "addressSpaceOverheadBytes": 2147483648,
                    "fileSizeLimitBytes": 536870912,
                    "openFileLimit": 1024,
                    "processLimit": 2,
                },
                "sandbox": {
                    "systemLibraries": [
                        {"identity": identity, "path": identity} for identity in sorted(system)
                    ],
                    "network": "denyIp",
                    "childProcesses": "exactCompatibilityChild",
                    "compatibilityChild": {
                        "executable": "container/LibreOffice.app/Contents/MacOS/soffice",
                        "maximumInstances": 1,
                        "localIp": "deny",
                        "localIpc": "exactEffectiveUidSessionUnixSocketOnly",
                    },
                },
            }
        },
    }
    destination = runtime / "authority.json"
    write_json(destination, value)
    return destination


def find_kit(files: list[pathlib.Path]) -> pathlib.Path:
    matches = [path for path in files if path.as_posix().endswith("/Contents/Frameworks/libmergedlo.dylib")]
    if len(matches) != 1:
        raise ReleaseError("reviewed LibreOfficeKit library path is absent")
    symbols = run(["/usr/bin/nm", "-gU", str(matches[0])])
    if not any(line.rstrip().endswith(" _libreofficekit_hook_2") for line in symbols.splitlines()):
        raise ReleaseError("reviewed LibreOfficeKit ABI export is absent")
    return matches[0]


def dependency_closure(worker: pathlib.Path, kit: pathlib.Path, root: pathlib.Path) -> set[str]:
    system: set[str] = set()
    pending = [worker, kit]
    visited: set[pathlib.Path] = set()
    while pending:
        owner = pending.pop()
        if owner in visited:
            continue
        visited.add(owner)
        needed, search = load_commands(owner)
        for identity in needed:
            if identity.startswith(SYSTEM_PREFIXES):
                system.add(identity)
                continue
            candidates = resolve_dependency(root, owner, identity, search)
            if len(candidates) != 1:
                raise ReleaseError(f"LibreOffice dependency is absent or ambiguous: {identity}")
            pending.append(candidates[0])
    return system


def load_commands(path: pathlib.Path) -> tuple[list[str], list[str]]:
    if not is_macho(path):
        raise ReleaseError(f"runtime dependency closure contains non-Mach-O file: {path.name}")
    lines = run(["/usr/bin/otool", "-L", str(path)]).splitlines()[1:]
    needed = [line.strip().split(" (compatibility", 1)[0] for line in lines if line.strip()]
    if needed and pathlib.PurePosixPath(needed[0]).name == path.name:
        needed.pop(0)
    commands = run(["/usr/bin/otool", "-l", str(path)]).splitlines()
    search = []
    for index, line in enumerate(commands):
        if line.strip() == "cmd LC_RPATH" and index + 2 < len(commands):
            value = commands[index + 2].strip()
            if value.startswith("path "):
                search.append(value[5:].split(" (offset", 1)[0])
    return needed, search


def resolve_dependency(
    root: pathlib.Path,
    owner: pathlib.Path,
    identity: str,
    search: list[str],
) -> list[pathlib.Path]:
    relative_owner = owner.relative_to(root)
    owner_dir = relative_owner.parent
    candidates: list[pathlib.Path] = []
    if identity.startswith("@loader_path/"):
        candidates.append(root / normalize(owner_dir / identity.removeprefix("@loader_path/")))
    elif identity.startswith("@rpath/"):
        suffix = identity.removeprefix("@rpath/")
        for base in search:
            if base == "@loader_path":
                expanded = owner_dir
            elif base.startswith("@loader_path/"):
                expanded = normalize(owner_dir / base.removeprefix("@loader_path/"))
            else:
                raise ReleaseError(f"LibreOffice has an unsupported rpath: {base}")
            candidates.append(root / normalize(expanded / suffix))
    elif identity.startswith("@executable_path"):
        raise ReleaseError("LibreOffice dependency uses forbidden @executable_path")
    elif "/" not in identity:
        candidates.append(root / normalize(owner_dir / identity))
    else:
        raise ReleaseError(f"LibreOffice dependency has an unsafe identity: {identity}")
    return [candidate for candidate in candidates if candidate.is_file() and not candidate.is_symlink()]


def normalize(path: pathlib.PurePath) -> pathlib.PurePath:
    parts: list[str] = []
    for part in path.parts:
        if part in {"", "."}:
            continue
        if part == "..":
            if not parts:
                raise ReleaseError("LibreOffice dependency escapes its private root")
            parts.pop()
        else:
            parts.append(part)
    return pathlib.PurePath(*parts)


def is_macho(path: pathlib.Path) -> bool:
    with path.open("rb") as source:
        magic = source.read(4)
    return magic in {
        b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf",
        b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca",
        b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca",
    }
