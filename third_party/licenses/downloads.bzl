"""Creates hash-pinned manual download repositories from downloads.json."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive", "http_file")

def _downloads_impl(module_ctx):
    manifest = json.decode(module_ctx.read(Label("//third_party/licenses:downloads.json")))
    if manifest.get("schema_version") != 1:
        fail("unsupported third-party download manifest schema")

    for item in manifest.get("model_files", []):
        http_file(
            name = item["repository"],
            downloaded_file_path = item["downloaded_file_path"],
            sha256 = item["sha256"],
            urls = [item["url"]],
        )

    # Runtime model files are intentionally empty until generated ONNX artifacts,
    # character tables, licenses, hashes, sizes, and platform coverage are reviewed.
    # When populated, they remain manual and use the same authoritative manifest.
    for item in manifest.get("model_runtime_files", []):
        http_file(
            name = item["repository"],
            downloaded_file_path = item["downloaded_file_path"],
            sha256 = item["sha256"],
            urls = [item["url"]],
        )

    for item in manifest.get("native_archives", []):
        http_archive(
            name = item["repository"],
            build_file_content = "exports_files(glob(['**']), visibility = ['//visibility:public'])",
            sha256 = item["sha256"],
            strip_prefix = item["strip_prefix"],
            urls = [item["url"]],
        )

downloads = module_extension(implementation = _downloads_impl)
