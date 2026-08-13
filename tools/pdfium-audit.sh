#!/bin/sh
set -eu

if [ "${PDFIUM_AUDIT_NETWORK:-}" != 1 ]; then
  echo "PDFium audit is networked and opt-in; set PDFIUM_AUDIT_NETWORK=1" >&2
  exit 2
fi

audit_dir=$(mktemp -d)
trap 'rm -rf "$audit_dir"' EXIT HUP INT TERM
manifest_file=third_party/pdfium/manifest.json
base=$(jq -er '.release_download_base' "$manifest_file")
if command -v llvm-objdump >/dev/null 2>&1; then
  objdump_tool=llvm-objdump
elif command -v objdump >/dev/null 2>&1; then
  objdump_tool=objdump
else
  echo "PDFium audit requires llvm-objdump or objdump" >&2
  exit 2
fi
if command -v llvm-nm >/dev/null 2>&1; then nm_tool=llvm-nm; else nm_tool=nm; fi

audit_one() {
  asset=$1 archive_size=$2 archive_sha=$3 library=$4 library_size=$5 library_sha=$6 pattern=$7 expected_dependencies=$8
  curl -fsSL --proto '=https' --tlsv1.2 --retry 5 --retry-all-errors --retry-delay 1 \
    --max-filesize 10000000 "$base/$asset" -o "$audit_dir/$asset"
  test "$(wc -c <"$audit_dir/$asset" | tr -d ' ')" = "$archive_size"
  printf '%s  %s\n' "$archive_sha" "$audit_dir/$asset" | shasum -a 256 -c -
  target_dir="$audit_dir/${asset%.tgz}"
  mkdir "$target_dir"
  tar -xzf "$audit_dir/$asset" -C "$target_dir"
  test "$(wc -c <"$target_dir/$library" | tr -d ' ')" = "$library_size"
  printf '%s  %s\n' "$library_sha" "$target_dir/$library" | shasum -a 256 -c -
  file "$target_dir/$library" | grep -Eq "$pattern"
  test -f "$target_dir/LICENSE"
  test -f "$target_dir/licenses/pdfium.txt"
  symbols_file="$target_dir/symbols.txt"
  (
    "$objdump_tool" -p "$target_dir/$library" 2>/dev/null || true
    "$objdump_tool" -t "$target_dir/$library" 2>/dev/null || true
    "$nm_tool" -g "$target_dir/$library" 2>/dev/null || true
  ) >"$symbols_file"
  jq -er '.required_exports[]' "$manifest_file" | while IFS= read -r symbol; do
    grep -q "$symbol" "$symbols_file"
  done
  case "$asset" in
    pdfium-linux-*) dependencies=$("$objdump_tool" -p "$target_dir/$library" | awk '$1 == "NEEDED" { print $2 }' | sort) ;;
    pdfium-mac-*)
      if command -v otool >/dev/null 2>&1; then
        dependencies=$(otool -L "$target_dir/$library" | tail -n +2 | awk '{ print $1 }' | grep -v '^\./libpdfium\.dylib$' | sort)
      elif [ "$objdump_tool" = llvm-objdump ]; then
        dependencies=$(llvm-objdump --macho --dylibs-used "$target_dir/$library" | tail -n +2 | grep -v '^\./libpdfium\.dylib$' | sort)
      else
        echo "Mach-O dependency audit requires otool or llvm-objdump" >&2
        exit 2
      fi
      ;;
    pdfium-win-*) dependencies=$("$objdump_tool" -p "$target_dir/$library" | awk '$1 == "DLL" && $2 == "Name:" { print $3 }' | sort) ;;
  esac
  expected=$(printf '%s' "$expected_dependencies" | tr ',' '\n' | sort)
  if [ "$dependencies" != "$expected" ]; then
    echo "unexpected dependency closure for $asset" >&2
    printf 'expected:\n%s\nactual:\n%s\n' "$expected" "$dependencies" >&2
    exit 1
  fi
}

tab=$(printf '\t')
jq -er '.targets | to_entries[] | [.value.asset, (.value.archive_size|tostring), .value.archive_sha256, .value.library, (.value.library_size|tostring), .value.library_sha256, .value.format_pattern, (.value.allowed_dependencies|join(","))] | @tsv' "$manifest_file" |
while IFS="$tab" read -r asset archive_size archive_sha library library_size library_sha pattern dependencies; do
  audit_one "$asset" "$archive_size" "$archive_sha" "$library" "$library_size" "$library_sha" "$pattern" "$dependencies"
done

if [ "${1:-}" = --native-smoke ]; then
  if [ "${PDFIUM_NATIVE_SMOKE:-}" != 1 ]; then echo "set PDFIUM_NATIVE_SMOKE=1" >&2; exit 2; fi
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) runtime="$audit_dir/pdfium-mac-arm64/lib/libpdfium.dylib" ;;
    *) echo "native smoke is supported on macOS ARM64 in this workflow" >&2; exit 2 ;;
  esac
  PDFIUM_LIBRARY="$runtime" cargo test -p into-markdown-pdfium native_smoke -- --ignored
fi

echo "PDFium four-platform artifact audit passed"
