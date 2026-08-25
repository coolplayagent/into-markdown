#!/usr/bin/env python3
"""Black-box acceptance for an installed Core and local official IMP packages."""

from __future__ import annotations

import argparse
import base64
import hashlib
import itertools
import json
import os
import pathlib
import queue
import shutil
import signal
import socket
import stat
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from typing import Any


PLUGINS = {
    "official.ocr.ppocrv6": ("ocr",),
    "official.media.whisper": ("transcription", "diarization"),
}
PLUGIN_MANAGER_AUTHORITY_FILES = frozenset({"plugin.json", ".installed.json", ".package.zip"})


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def tree_hash(root: pathlib.Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*"), key=lambda value: value.as_posix()):
        relative = path.relative_to(root).as_posix().encode()
        digest.update(len(relative).to_bytes(8, "big"))
        digest.update(relative)
        if path.is_symlink():
            raise RuntimeError(f"installed Core contains a link: {relative!r}")
        if path.is_file():
            digest.update(bytes.fromhex(sha256(path)))
    return digest.hexdigest()


def repairable_payload_files(installed_roots: list[pathlib.Path]) -> list[pathlib.Path]:
    """Return payload fixtures without corrupting manager rollback authority."""
    return sorted(
        path
        for root in installed_roots
        for path in root.rglob("*")
        if path.is_file()
        and path.stat().st_size > 0
        and path.name not in PLUGIN_MANAGER_AUTHORITY_FILES
    )


@dataclass
class Result:
    name: str
    passed: bool
    exit_code: int | None
    duration_ms: int
    detail: str


class ProcessOwner:
    """Own a process group on Unix or a kill-on-close Job Object on Windows."""

    def __init__(self, process: subprocess.Popen[str]):
        self.process = process
        self.job: int | None = None
        if os.name == "nt":
            import ctypes
            from ctypes import wintypes

            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
            kernel32.CreateJobObjectW.restype = wintypes.HANDLE
            kernel32.SetInformationJobObject.argtypes = [
                wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD,
            ]
            kernel32.SetInformationJobObject.restype = wintypes.BOOL
            kernel32.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
            kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
            kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
            kernel32.CloseHandle.restype = wintypes.BOOL
            kernel32.TerminateJobObject.argtypes = [wintypes.HANDLE, wintypes.UINT]
            kernel32.TerminateJobObject.restype = wintypes.BOOL
            handle = kernel32.CreateJobObjectW(None, None)
            if not handle:
                process.kill()
                raise OSError(ctypes.get_last_error(), "CreateJobObjectW")

            class BasicLimit(ctypes.Structure):
                _fields_ = [
                    ("PerProcessUserTimeLimit", ctypes.c_int64),
                    ("PerJobUserTimeLimit", ctypes.c_int64),
                    ("LimitFlags", wintypes.DWORD),
                    ("MinimumWorkingSetSize", ctypes.c_size_t),
                    ("MaximumWorkingSetSize", ctypes.c_size_t),
                    ("ActiveProcessLimit", wintypes.DWORD),
                    ("Affinity", ctypes.c_size_t),
                    ("PriorityClass", wintypes.DWORD),
                    ("SchedulingClass", wintypes.DWORD),
                ]

            class IoCounters(ctypes.Structure):
                _fields_ = [(name, ctypes.c_uint64) for name in (
                    "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
                    "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
                )]

            class ExtendedLimit(ctypes.Structure):
                _fields_ = [
                    ("BasicLimitInformation", BasicLimit),
                    ("IoInfo", IoCounters),
                    ("ProcessMemoryLimit", ctypes.c_size_t),
                    ("JobMemoryLimit", ctypes.c_size_t),
                    ("PeakProcessMemoryUsed", ctypes.c_size_t),
                    ("PeakJobMemoryUsed", ctypes.c_size_t),
                ]

            limits = ExtendedLimit()
            limits.BasicLimitInformation.LimitFlags = 0x00002000
            if not kernel32.SetInformationJobObject(handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
                kernel32.CloseHandle(handle)
                process.kill()
                raise OSError(ctypes.get_last_error(), "SetInformationJobObject")
            if not kernel32.AssignProcessToJobObject(handle, wintypes.HANDLE(process._handle)):
                kernel32.CloseHandle(handle)
                process.kill()
                raise OSError(ctypes.get_last_error(), "AssignProcessToJobObject")
            self.job = handle

    def terminate(self) -> None:
        if os.name == "nt" and self.job:
            import ctypes

            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.TerminateJobObject.argtypes = [ctypes.c_void_p, ctypes.c_uint]
            kernel32.TerminateJobObject(self.job, 1)
        elif self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)

    def close(self) -> None:
        if os.name == "nt" and self.job:
            import ctypes

            kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
            kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
            kernel32.CloseHandle(self.job)
            self.job = None


class Runner:
    def __init__(self, executable: pathlib.Path, work: pathlib.Path, timeout: int):
        self.executable = executable
        self.work = work
        self.timeout = timeout
        self.results: list[Result] = []
        self.prepared_states: set[pathlib.Path] = set()

    def environment(self, state: pathlib.Path) -> dict[str, str]:
        temporary = state / "tmp"
        isolated = [
            temporary,
            state / "xdg-config",
            state / "xdg-data",
            state / "user-data",
            state / "web-data",
            state / "appdata-roaming",
            state / "appdata-local",
        ]
        resolved_state = state.resolve()
        if resolved_state not in self.prepared_states:
            for directory in isolated:
                directory.mkdir(parents=True, exist_ok=True)
                if os.name == "nt":
                    protect_windows_directory(directory)
                else:
                    directory.chmod(0o700)
            self.prepared_states.add(resolved_state)
        user_data = str((state / "user-data").resolve())
        if os.name == "nt" and not user_data.startswith("\\\\?\\"):
            user_data = "\\\\?\\" + user_data
        result = {
            "HOME": str(state),
            "USERPROFILE": str(state),
            "XDG_CONFIG_HOME": str(state / "xdg-config"),
            "XDG_DATA_HOME": str(state / "xdg-data"),
            "INTO_MARKDOWN_USER_DATA_HOME": user_data,
            "TMPDIR": str(temporary),
            "TEMP": str(temporary),
            "TMP": str(temporary),
            "NO_COLOR": "1",
        }
        if os.name == "nt":
            result["APPDATA"] = str(state / "appdata-roaming")
            result["LOCALAPPDATA"] = str(state / "appdata-local")
        for name in ("SystemRoot", "WINDIR"):
            if name in os.environ:
                result[name] = os.environ[name]
        return result

    def call(self, name: str, state: pathlib.Path, arguments: list[str], *, succeed: bool = True) -> subprocess.CompletedProcess[str]:
        started = time.monotonic()
        creationflags = (
            subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW
            if os.name == "nt"
            else 0
        )
        process = subprocess.Popen(
            [str(self.executable), *arguments],
            cwd=self.work,
            env=self.environment(state),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            start_new_session=os.name != "nt",
            creationflags=creationflags,
        )
        owner = ProcessOwner(process)
        try:
            try:
                stdout, stderr = process.communicate(timeout=self.timeout)
            except subprocess.TimeoutExpired:
                owner.terminate()
                stdout, stderr = process.communicate()
                raise RuntimeError(f"{name}: deadline exceeded; stdout={stdout[-500:]!r}; stderr={stderr[-500:]!r}")
        finally:
            owner.close()
        passed = (process.returncode == 0) == succeed
        detail = (stderr if stderr.strip() else stdout).strip()[-2000:]
        self.results.append(Result(name, passed, process.returncode, int((time.monotonic() - started) * 1000), detail))
        if not passed:
            expectation = "success" if succeed else "failure"
            raise RuntimeError(f"{name}: expected {expectation}, exit={process.returncode}: {detail}")
        return subprocess.CompletedProcess(process.args, process.returncode, stdout, stderr)

    def running_snapshot(self, state: pathlib.Path, audio: pathlib.Path, plugin: str) -> None:
        name = "running-media-snapshot"
        started = time.monotonic()
        creationflags = (
            subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW
            if os.name == "nt"
            else 0
        )
        process = subprocess.Popen(
            [str(self.executable), str(audio), "--ai", "audio-transcription=only", "--emit", "result-json"],
            cwd=self.work,
            env=self.environment(state),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            start_new_session=os.name != "nt",
            creationflags=creationflags,
        )
        owner = ProcessOwner(process)
        try:
            time.sleep(1)
            self.call("running-media-disable", state, ["plugins", "disable", plugin, "--scope", "global"])
            try:
                stdout, stderr = process.communicate(timeout=self.timeout)
            except subprocess.TimeoutExpired:
                owner.terminate()
                stdout, stderr = process.communicate()
                raise RuntimeError(f"{name}: immutable snapshot did not complete")
        finally:
            owner.close()
        passed = process.returncode == 0
        self.results.append(Result(name, passed, process.returncode, int((time.monotonic() - started) * 1000), (stderr or stdout).strip()[-2000:]))
        if not passed:
            raise RuntimeError(f"{name}: existing task did not complete from its immutable snapshot")
        self.call(
            "running-media-new-task-disabled",
            state,
            [str(audio), "--ai", "audio-transcription=only", "--emit", "result-json"],
            succeed=False,
        )
        self.call("running-media-enable", state, ["plugins", "enable", plugin, "--scope", "global"])


class WebSession:
    """Drive the installed production Web bundle through its authenticated loopback API."""

    def __init__(self, runner: Runner, state: pathlib.Path):
        self.runner = runner
        self.state = state
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]
        creationflags = (
            subprocess.CREATE_NEW_PROCESS_GROUP | subprocess.CREATE_NO_WINDOW
            if os.name == "nt"
            else 0
        )
        started = time.monotonic()
        self.process = subprocess.Popen(
            [
                str(runner.executable), "ui", "--port", str(self.port), "--no-open",
                "--data-dir", str((state / "web-data").resolve()),
            ],
            cwd=runner.work,
            env=runner.environment(state),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            start_new_session=os.name != "nt",
            creationflags=creationflags,
        )
        self.owner = ProcessOwner(self.process)
        lines: queue.Queue[str] = queue.Queue()

        def read_lines() -> None:
            assert self.process.stdout is not None
            for line in self.process.stdout:
                lines.put(line)

        threading.Thread(target=read_lines, daemon=True).start()
        deadline = time.monotonic() + runner.timeout
        self.session = ""
        detail: list[str] = []
        while time.monotonic() < deadline and self.process.poll() is None:
            try:
                line = lines.get(timeout=0.2)
            except queue.Empty:
                continue
            detail.append(line.strip())
            marker = "#into-md-session="
            if marker in line:
                self.session = line.split(marker, 1)[1].strip()
                break
        passed = bool(self.session) and self.process.poll() is None
        runner.results.append(Result(
            "web-production-bundle-start",
            passed,
            self.process.poll(),
            int((time.monotonic() - started) * 1000),
            "\n".join(detail)[-2000:],
        ))
        if not passed:
            self.close()
            stderr = self.process.stderr.read() if self.process.stderr is not None else ""
            raise RuntimeError(
                f"installed Web service did not publish a private session URL: {stderr[-2000:]}"
            )
        self.origin = f"http://127.0.0.1:{self.port}"
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def request(
        self,
        name: str,
        path: str,
        *,
        method: str = "GET",
        body: bytes | None = None,
        content_type: str | None = None,
        headers: dict[str, str] | None = None,
        succeed: bool = True,
    ) -> Any:
        started = time.monotonic()
        request_headers = {
            "Origin": self.origin,
            "X-Into-Md-Session": self.session,
        }
        if content_type:
            request_headers["Content-Type"] = content_type
        if headers:
            request_headers.update(headers)
        request = urllib.request.Request(
            f"{self.origin}{path}", data=body, headers=request_headers, method=method,
        )
        status = None
        payload = b""
        try:
            with self.opener.open(request, timeout=self.runner.timeout) as response:
                status = response.status
                payload = response.read()
        except urllib.error.HTTPError as error:
            status = error.code
            payload = error.read()
        except Exception as error:
            self.runner.results.append(Result(
                name, False, None, int((time.monotonic() - started) * 1000), str(error),
            ))
            raise RuntimeError(f"{name}: Web request failed: {error}") from error
        passed = (status is not None and 200 <= status < 300) == succeed
        decoded = payload.decode("utf-8", errors="replace")
        detail = decoded[-2000:]
        self.runner.results.append(Result(
            name, passed, status, int((time.monotonic() - started) * 1000), detail,
        ))
        if not passed:
            expectation = "success" if succeed else "failure"
            raise RuntimeError(f"{name}: expected {expectation}, HTTP {status}: {detail}")
        if not payload:
            return None
        try:
            return json.loads(payload)
        except json.JSONDecodeError:
            return decoded

    def dangerous_action(self, name: str, action: dict[str, Any]) -> Any:
        authorized = {**action, "authorizeDangerous": True}
        grant = self.request(
            f"{name}-grant", "/api/admin/grant", method="POST",
            body=json.dumps(authorized).encode(), content_type="application/json",
        )
        authorized["authorizationGrant"] = grant["grant"]
        return self.request(
            name, "/api/admin", method="POST",
            body=json.dumps(authorized).encode(), content_type="application/json",
        )

    def stage(self, name: str, package: pathlib.Path) -> str:
        filename = base64.urlsafe_b64encode(package.name.encode()).decode().rstrip("=")
        staged = self.request(
            name, "/api/admin/plugin-package", method="POST", body=package.read_bytes(),
            content_type="application/octet-stream",
            headers={"X-Into-Md-Plugin-Filename-B64": filename},
        )
        return str(staged["source"])

    def close(self) -> None:
        started = time.monotonic()
        self.owner.terminate()
        try:
            self.process.wait(timeout=10)
            passed = True
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            passed = False
        self.owner.close()
        self.runner.results.append(Result(
            "web-process-tree-cleanup", passed, self.process.returncode,
            int((time.monotonic() - started) * 1000), "process group terminated",
        ))


def web_lifecycle(
    runner: Runner,
    state: pathlib.Path,
    packages: dict[str, pathlib.Path],
    publisher: dict[str, Any],
) -> None:
    web = WebSession(runner, state)
    try:
        index = web.request("web-production-index", "/")
        if not isinstance(index, str) or "<script" not in index or "/assets/" not in index:
            raise RuntimeError("installed Web root is not the production application bundle")
        for plugin, package in packages.items():
            def install_action(label: str) -> None:
                source = web.stage(f"{label}-stage", package)
                web.dangerous_action(label, {
                    "schemaVersion": 1,
                    "action": "plugin.install",
                    "scope": "global",
                    "target": plugin,
                    "source": source,
                    "sha256": sha256(package),
                    "signingKeyId": publisher["signingKeyId"],
                    "signingKeySha256": publisher["signingKeySha256"],
                })

            install_action(f"web-install-{plugin}")
            install_action(f"web-idempotent-reinstall-{plugin}")
            snapshot = web.request(f"web-refresh-after-install-{plugin}", "/api/admin")
            if not any(item.get("id") == plugin for item in snapshot.get("plugins", [])):
                raise RuntimeError(f"Web status did not refresh installed plugin {plugin}")
            verify = {
                "schemaVersion": 1, "action": "plugin.verify", "scope": "global", "target": plugin,
            }
            web.request(
                f"web-verify-{plugin}", "/api/admin", method="POST",
                body=json.dumps(verify).encode(), content_type="application/json",
            )
            for action in ("plugin.disable", "plugin.enable"):
                web.dangerous_action(f"web-{action.split('.')[1]}-{plugin}", {
                    "schemaVersion": 1, "action": action, "scope": "global", "target": plugin,
                })
            installed_roots = [
                metadata.parent for metadata in state.rglob(".installed.json")
                if plugin in metadata.as_posix()
            ]
            candidates = repairable_payload_files(installed_roots)
            if not candidates:
                raise RuntimeError(f"Web-installed package {plugin} has no repair fixture")
            damaged = sorted(candidates)[0]
            damaged.chmod(damaged.stat().st_mode | stat.S_IWRITE)
            with damaged.open("r+b") as stream:
                first = stream.read(1)
                stream.seek(0)
                stream.write(bytes([first[0] ^ 0x80]))
            web.request(
                f"web-damaged-verify-{plugin}", "/api/admin", method="POST",
                body=json.dumps(verify).encode(), content_type="application/json", succeed=False,
            )
            install_action(f"web-repair-{plugin}")
            web.request(
                f"web-repaired-verify-{plugin}", "/api/admin", method="POST",
                body=json.dumps(verify).encode(), content_type="application/json",
            )
            web.dangerous_action(f"web-remove-{plugin}", {
                "schemaVersion": 1, "action": "plugin.remove", "scope": "global", "target": plugin,
            })
            refreshed = web.request(f"web-refresh-after-remove-{plugin}", "/api/admin")
            if any(item.get("id") == plugin for item in refreshed.get("plugins", [])):
                raise RuntimeError(f"Web status still exposes removed plugin {plugin}")
    finally:
        web.close()


def protect_windows_directory(path: pathlib.Path) -> None:
    identity = "\\".join(
        part for part in (os.environ.get("USERDOMAIN"), os.environ.get("USERNAME")) if part
    )
    if not identity:
        raise RuntimeError("current Windows identity is unavailable")
    system_root = pathlib.Path(os.environ.get("SystemRoot", r"C:\Windows"))
    icacls = system_root / "System32" / "icacls.exe"
    result = subprocess.run(
        [str(icacls), str(path), "/inheritance:r", "/grant:r", f"{identity}:(OI)(CI)F"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        creationflags=subprocess.CREATE_NO_WINDOW,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(f"cannot protect Windows acceptance directory: {result.stderr.strip()}")


def private_directory(path: pathlib.Path) -> pathlib.Path:
    path.mkdir(parents=True, exist_ok=False)
    if os.name == "nt":
        protect_windows_directory(path)
    else:
        path.chmod(0o700)
    return path.resolve(strict=True)


def capability_map(payload: str) -> dict[str, dict[str, Any]]:
    value = json.loads(payload)
    items = value.get("capabilities", value if isinstance(value, list) else [])
    return {str(item["id"]): item for item in items}


def legacy_windows_path(value: str) -> str:
    """Remove only the two Win32 verbatim prefixes after the CLI has emitted an absolute path."""
    if value.startswith("\\\\?\\UNC\\"):
        return "\\\\" + value[8:]
    if value.startswith("\\\\?\\"):
        return value[4:]
    return value


def install(runner: Runner, state: pathlib.Path, package: pathlib.Path, publisher: dict[str, Any], label: str) -> pathlib.Path:
    output = runner.call(
        label,
        state,
        [
            "plugins", "install", str(package), "--sha256", sha256(package),
            "--signing-key-id", publisher["signingKeyId"],
            "--signing-key-sha256", publisher["signingKeySha256"],
            "--scope", "global",
        ],
    )
    lines = output.stdout.strip().splitlines()
    columns = lines[-1].split("\t") if lines else []
    if len(columns) != 2 or columns[0] not in PLUGINS:
        raise RuntimeError(f"{label}: CLI returned an invalid plugin installation result")
    reported = legacy_windows_path(columns[1]) if os.name == "nt" else columns[1]
    config = pathlib.Path(reported).resolve(strict=True)
    expected_config = (state / "user-data" / "into-markdown" / "config.toml").resolve(strict=True)
    if not config.is_file() or not config.samefile(expected_config):
        raise RuntimeError(f"{label}: CLI returned a configuration path outside isolated state")
    installed = (expected_config.parent / "plugins" / columns[0]).resolve(strict=True)
    resolved_state = state.resolve(strict=True)
    if not installed.is_dir() or not installed.is_relative_to(resolved_state):
        raise RuntimeError(f"{label}: installed plugin path is outside isolated state")
    return installed


def assert_states(runner: Runner, state: pathlib.Path, selected: set[str], label: str) -> dict[str, dict[str, Any]]:
    output = runner.call(label, state, ["capabilities", "list", "--json"])
    capabilities = capability_map(output.stdout)
    text = json.dumps(capabilities, sort_keys=True)
    for plugin, ids in PLUGINS.items():
        for capability in ids:
            if capability not in capabilities:
                raise RuntimeError(f"{label}: capability {capability!r} absent")
            ready = capabilities[capability].get("status") == "ready"
            if ready != (plugin in selected):
                raise RuntimeError(f"{label}: {capability} status was {capabilities[capability].get('status')!r}")
            if plugin not in selected and plugin not in text and "setup" not in text:
                raise RuntimeError(f"{label}: missing capability lacks its exact install entry")
    return capabilities


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--install-root", type=pathlib.Path, required=True)
    parser.add_argument("--into-md", type=pathlib.Path, required=True)
    parser.add_argument("--plugins", type=pathlib.Path, required=True)
    parser.add_argument("--publisher", type=pathlib.Path, required=True)
    parser.add_argument("--fixtures", type=pathlib.Path, required=True)
    parser.add_argument("--audio-fixture", type=pathlib.Path, required=True)
    parser.add_argument("--work-root", type=pathlib.Path, required=True)
    parser.add_argument("--archive-sha256", required=True)
    parser.add_argument("--platform-audit", type=pathlib.Path, required=True)
    parser.add_argument("--report", type=pathlib.Path, required=True)
    parser.add_argument("--timeout-seconds", type=int, default=120)
    return parser.parse_args()


def main() -> int:
    arguments = parse_arguments()
    install_root = arguments.install_root.resolve(strict=True)
    executable = arguments.into_md.resolve(strict=True)
    plugins_root = arguments.plugins.resolve(strict=True)
    fixtures = arguments.fixtures.resolve(strict=True)
    audio = arguments.audio_fixture.resolve(strict=True)
    publisher = json.loads(arguments.publisher.read_text(encoding="utf-8"))
    platform_audit = json.loads(arguments.platform_audit.read_text(encoding="utf-8"))
    if len(arguments.archive_sha256) != 64 or any(character not in "0123456789abcdef" for character in arguments.archive_sha256):
        raise SystemExit("archive SHA-256 must be 64 lowercase hexadecimal characters")
    packages = {plugin: (plugins_root / f"{plugin}.imp").resolve(strict=True) for plugin in PLUGINS}
    work = private_directory(arguments.work_root)
    runner = Runner(executable, work, arguments.timeout_seconds)
    before = tree_hash(install_root)
    matrix: list[dict[str, Any]] = []
    conclusion = "failed"
    error = None
    try:
        runner.call("core-version", private_directory(work / "core-state"), ["--version"])
        plugin_names = list(PLUGINS)
        for mask in range(1 << len(plugin_names)):
            selected = {plugin for index, plugin in enumerate(plugin_names) if mask & (1 << index)}
            state = private_directory(work / f"combination-{mask}")
            for plugin in plugin_names:
                if plugin in selected:
                    install(runner, state, packages[plugin], publisher, f"combination-{mask}-install-{plugin}")
                    runner.call(f"combination-{mask}-verify-{plugin}", state, ["plugins", "verify", plugin, "--scope", "global", "--json"])
            statuses = assert_states(runner, state, selected, f"combination-{mask}-statuses")
            matrix.append({"kind": "combination", "selected": sorted(selected), "statuses": statuses, "passed": True})
        for index, order in enumerate(itertools.permutations(plugin_names)):
            state = private_directory(work / f"order-{index}")
            for plugin in order:
                install(runner, state, packages[plugin], publisher, f"order-{index}-install-{plugin}")
                assert_states(runner, state, set(order[: order.index(plugin) + 1]), f"order-{index}-after-{plugin}")
            matrix.append({"kind": "order", "order": list(order), "passed": True})

        lifecycle = private_directory(work / "lifecycle")
        for plugin in plugin_names:
            unchanged = tree_hash(install_root)
            installed_plugin = install(runner, lifecycle, packages[plugin], publisher, f"lifecycle-install-{plugin}")
            install(runner, lifecycle, packages[plugin], publisher, f"lifecycle-idempotent-reinstall-{plugin}")
            resource = next(iter(repairable_payload_files([installed_plugin])), None)
            if resource is None:
                raise RuntimeError(f"{plugin}: installed package has no corruptible declared resource")
            resource.chmod(resource.stat().st_mode | stat.S_IWRITE)
            with resource.open("r+b") as value:
                first = value.read(1)
                value.seek(0)
                value.write(bytes([first[0] ^ 0x80]))
            runner.call(f"lifecycle-damaged-verify-{plugin}", lifecycle, ["plugins", "verify", plugin, "--scope", "global", "--json"], succeed=False)
            install(runner, lifecycle, packages[plugin], publisher, f"lifecycle-repair-{plugin}")
            runner.call(f"lifecycle-repaired-verify-{plugin}", lifecycle, ["plugins", "verify", plugin, "--scope", "global", "--json"])
            runner.call(f"lifecycle-disable-{plugin}", lifecycle, ["plugins", "disable", plugin, "--scope", "global"])
            runner.call(f"lifecycle-verify-disabled-{plugin}", lifecycle, ["plugins", "verify", plugin, "--scope", "global", "--json"])
            runner.call(f"lifecycle-enable-{plugin}", lifecycle, ["plugins", "enable", plugin, "--scope", "global"])
            runner.call(f"lifecycle-remove-{plugin}", lifecycle, ["plugins", "remove", plugin, "--scope", "global"])
            install(runner, lifecycle, packages[plugin], publisher, f"lifecycle-reinstall-{plugin}")
            if tree_hash(install_root) != unchanged:
                raise RuntimeError(f"Core changed during {plugin} lifecycle")

        fixture_state = private_directory(lifecycle / "fixtures")
        ocr_fixture = fixture_state / "ocr-english-clear-1.png"
        speech_fixture = fixture_state / audio.name
        shutil.copyfile(fixtures / "ocr" / "ocr-english-clear-1.png", ocr_fixture)
        shutil.copyfile(audio, speech_fixture)
        runner.call(
            "real-ocr-fixture",
            lifecycle,
            [str(ocr_fixture), "--ocr", "always", "--emit", "result-json"],
        )
        runner.call(
            "real-speech-fixture",
            lifecycle,
            [str(speech_fixture), "--ai", "audio-transcription=only", "--emit", "result-json"],
        )
        runner.running_snapshot(lifecycle, speech_fixture, "official.media.whisper")

        web_state = private_directory(work / "web-lifecycle")
        web_lifecycle(runner, web_state, packages, publisher)

        faults = private_directory(work / "faults")
        first = packages[plugin_names[0]]
        runner.call(
            "failure-wrong-summary", faults,
            ["plugins", "install", str(first), "--sha256", "0" * 64, "--signing-key-id", publisher["signingKeyId"], "--signing-key-sha256", publisher["signingKeySha256"], "--scope", "global"],
            succeed=False,
        )
        corrupt = work / "corrupt.imp"
        shutil.copyfile(first, corrupt)
        with corrupt.open("r+b") as output:
            output.seek(max(0, corrupt.stat().st_size // 2))
            byte = output.read(1)
            output.seek(-1, os.SEEK_CUR)
            output.write(bytes([byte[0] ^ 0x80]))
        runner.call(
            "failure-corrupt-package", faults,
            ["plugins", "install", str(corrupt), "--sha256", sha256(corrupt), "--signing-key-id", publisher["signingKeyId"], "--signing-key-sha256", publisher["signingKeySha256"], "--scope", "global"],
            succeed=False,
        )
        # Real fixtures are explicit acceptance inputs; checking them here prevents a source-tree
        # fallback from making a missing installed fixture or licensed audio input look successful.
        if not fixtures.is_dir() or not audio.is_file():
            raise RuntimeError("real acceptance fixtures are unavailable")
        conclusion = "passed"
    except Exception as caught:
        error = str(caught)
    after = tree_hash(install_root)
    residual = sorted(path.relative_to(work).as_posix() for path in work.rglob("*") if path.name.startswith((".install-", ".remove-", ".transaction")))
    report = {
        "schemaVersion": 1,
        "target": arguments.target,
        "artifactSha256": {
            "core": arguments.archive_sha256,
            **{plugin: sha256(path) for plugin, path in packages.items()},
        },
        "coreInstallStatus": {"before": before, "after": after, "unchanged": before == after},
        "pluginMatrix": matrix,
        "platformAudit": platform_audit,
        "cases": [result.__dict__ for result in runner.results],
        "residualResources": residual,
        "conclusion": conclusion,
        "error": error,
    }
    arguments.report.parent.mkdir(parents=True, exist_ok=True)
    arguments.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    if conclusion != "passed" or before != after or residual or not platform_audit.get("passed"):
        raise SystemExit(error or "installed platform acceptance failed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
