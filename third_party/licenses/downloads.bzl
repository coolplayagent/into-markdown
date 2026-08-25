"""Creates hash-pinned manual download repositories from downloads.json."""

load("@bazel_tools//tools/build_defs/repo:http.bzl", "http_archive", "http_file")

def _downloads_impl(module_ctx):
    manifest = json.decode(module_ctx.read(Label("//third_party/licenses:downloads.json")))
    pdfium_manifest = json.decode(module_ctx.read(Label("//third_party/pdfium:manifest.json")))
    if manifest.get("schema_version") != 1:
        fail("unsupported third-party download manifest schema")

    for item in manifest.get("model_files", []):
        http_file(
            name = item["repository"],
            downloaded_file_path = item["downloaded_file_path"],
            sha256 = item["sha256"],
            urls = [item["url"]] + item.get("mirror_urls", []),
        )

    # Runtime model files are intentionally empty until generated ONNX artifacts,
    # character tables, licenses, hashes, sizes, and platform coverage are reviewed.
    # When populated, they remain manual and use the same authoritative manifest.
    for item in manifest.get("model_runtime_files", []):
        if item.get("archive_sha256"):
            http_archive(
                name = item["repository"],
                build_file_content = "exports_files(glob(['**']), visibility = ['//visibility:public'])",
                sha256 = item["archive_sha256"],
                urls = [item["url"]] + item.get("mirror_urls", []),
            )
            http_file(
                name = item["repository"] + "_archive",
                downloaded_file_path = "runtime-model.tar",
                sha256 = item["archive_sha256"],
                urls = [item["url"]] + item.get("mirror_urls", []),
            )
        else:
            http_file(
                name = item["repository"],
                downloaded_file_path = item["downloaded_file_path"],
                sha256 = item["sha256"],
                urls = [item["url"]] + item.get("mirror_urls", []),
            )

    for item in manifest.get("native_archives", []):
        args = {
            "build_file_content": "exports_files(glob(['**']), visibility = ['//visibility:public'])",
            "sha256": item["sha256"],
            "urls": [item["url"]],
        }
        if item.get("strip_prefix"):
            args["strip_prefix"] = item["strip_prefix"]
        http_archive(name = item["repository"], **args)

    for item in manifest.get("pdfium_archives", []):
        target = pdfium_manifest.get("targets", {}).get(item["target"])
        if not target or target.get("archive_sha256") != item["sha256"]:
            fail("PDFium download and runtime authorities disagree for " + item["target"])
        library = target.get("library")
        if not library:
            fail("PDFium runtime authority is missing its library path for " + item["target"])
        http_archive(
            name = item["repository"],
            build_file_content = """
exports_files(glob(["**"]), visibility = ["//visibility:public"])
filegroup(
    name = "distribution",
    srcs = ["%s"] + glob(["LICENSE", "licenses/**"]),
    visibility = ["//visibility:public"],
)
""" % library,
            sha256 = item["sha256"],
            urls = [item["url"]],
        )

downloads = module_extension(implementation = _downloads_impl)
