#!/bin/sh
set -eu

fail() { printf '%s\n' "FFmpeg audit: $*" >&2; exit 1; }

if [ "${FFMPEG_AUDIT_NETWORK:-}" != 1 ]; then
  echo "FFmpeg source build is networked and opt-in; set FFMPEG_AUDIT_NETWORK=1" >&2
  exit 2
fi
command -v jq >/dev/null
for audit_tool in curl gpg jq make file python3 shasum; do command -v "$audit_tool" >/dev/null; done
curl_retry_all_errors=
if curl --help all 2>/dev/null | grep -q -- '--retry-all-errors'; then
  curl_retry_all_errors=--retry-all-errors
fi
download() {
  max_bytes=$1
  download_url=$2
  destination=$3
  set -- -fsSL --proto '=https' --tlsv1.2 --retry 8 --retry-delay 1
  if [ -n "$curl_retry_all_errors" ]; then set -- "$@" "$curl_retry_all_errors"; fi
  if [ "$max_bytes" -gt 0 ]; then set -- "$@" --max-filesize "$max_bytes"; fi
  curl "$@" "$download_url" -o "$destination"
}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_manifest="$root/third_party/ffmpeg/source.json"
build_policy="$root/third_party/ffmpeg/build-policy.json"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM
output_dir=${FFMPEG_AUDIT_OUTPUT_DIR:-$work/output}
mkdir -p "$output_dir"
if find "$output_dir" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
  echo "FFmpeg audit output directory must be empty" >&2; exit 2
fi
url=$(jq -er .source_url "$source_manifest")
source_sha=$(jq -er .source_sha256 "$source_manifest")
source_bytes=$(jq -er .source_bytes "$source_manifest")
version=$(jq -er .version "$source_manifest")
test "$(jq -er .ffmpeg_version "$build_policy")" = "$version"
sig_url=$(jq -er .signature_url "$source_manifest")
sig_sha=$(jq -er .signature_sha256 "$source_manifest")
if [ -n "${FFMPEG_AUDIT_SOURCE:-}" ]; then cp "$FFMPEG_AUDIT_SOURCE" "$work/source.tar.xz"; else
  download 12000000 "$url" "$work/source.tar.xz"
fi
test "$(wc -c < "$work/source.tar.xz" | tr -d ' ')" = "$source_bytes"
printf '%s  %s\n' "$source_sha" "$work/source.tar.xz" | shasum -a 256 -c -
if [ -n "${FFMPEG_AUDIT_SIGNATURE:-}" ]; then cp "$FFMPEG_AUDIT_SIGNATURE" "$work/source.asc"; else
  download 1024 "$sig_url" "$work/source.asc"
fi
printf '%s  %s\n' "$sig_sha" "$work/source.asc" | shasum -a 256 -c -
command -v gpg >/dev/null
GNUPGHOME="$work/gnupg"; export GNUPGHOME
case "$(uname -s)" in
  MINGW*|MSYS*)
    mkdir "$GNUPGHOME"
    windows_gnupg=$(cygpath -w "$GNUPGHOME")
    windows_owner=$(whoami.exe | tr -d '\r')
    MSYS2_ARG_CONV_EXCL='*' icacls.exe "$windows_gnupg" /inheritance:r /grant:r "$windows_owner:(OI)(CI)F" >/dev/null
    ;;
  *) mkdir -m 700 "$GNUPGHOME" ;;
esac
key_url=$(jq -er .signing_key_url "$source_manifest")
key_sha=$(jq -er .signing_key_sha256 "$source_manifest")
download 4096 "$key_url" "$work/signing-key.asc"
printf '%s  %s\n' "$key_sha" "$work/signing-key.asc" | shasum -a 256 -c -
gpg --batch --import "$work/signing-key.asc"
test "$(gpg --batch --with-colons --fingerprint FCF986EA15E6E293A5644F10B4322F04D67658D8 | awk -F: '$1 == "fpr" {print $10; exit}')" = FCF986EA15E6E293A5644F10B4322F04D67658D8
gpg --batch --verify "$work/source.asc" "$work/source.tar.xz"
tar -xJf "$work/source.tar.xz" -C "$work"
src="$work/ffmpeg-$version"
test -s "$src/COPYING.LGPLv2.1" || fail "verified source archive is missing COPYING.LGPLv2.1"
grep -q 'GNU LESSER GENERAL PUBLIC LICENSE' "$src/COPYING.LGPLv2.1" \
  || fail "verified source archive has an unexpected LGPL license"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin; format=mach-o; arch=aarch64; toolchain_args='--extra-cflags=-mmacosx-version-min=14.0 --extra-ldflags=-mmacosx-version-min=14.0' ;;
  Linux-x86_64)
    command -v nasm >/dev/null || fail "nasm is required for the x86_64 optimized build"
    target=x86_64-unknown-linux-gnu; format=elf; arch=x86_64; toolchain_args=
    ;;
  Linux-aarch64) target=aarch64-unknown-linux-gnu; format=elf; arch=aarch64; toolchain_args= ;;
  MINGW*-x86_64|MSYS*-x86_64)
    command -v objdump >/dev/null || fail "objdump is unavailable in the MSYS2 release shell"
    target=x86_64-pc-windows-msvc; format=pe; arch=x86_64
    toolchain_args='--toolchain=msvc --disable-x86asm'
    msvc_tools=$(jq -er '.targets["x86_64-pc-windows-msvc"].buildBaseline.msvcTools' "$root/tools/platform-release/authority.json")
    if command -v cl.exe >/dev/null 2>&1; then
      INTO_MD_REAL_CL=$(command -v cl.exe)
    elif [ -n "${VCToolsInstallDir:-}" ]; then
      INTO_MD_REAL_CL=$(cygpath -u "$VCToolsInstallDir")/bin/HostX64/x64/cl.exe
      test -f "$INTO_MD_REAL_CL" \
        || fail "cl.exe is missing from fixed VCToolsInstallDir: $INTO_MD_REAL_CL"
    else
      fail "cl.exe is unavailable and VCToolsInstallDir is not set in the MSYS2 release shell"
    fi
    export INTO_MD_REAL_CL
    # setup-msys2 intentionally starts with a minimal POSIX PATH. FFmpeg's
    # MSVC linker adapter invokes link by name, so make the fixed VC bin
    # directory authoritative over MSYS2's unrelated /usr/bin/link utility.
    PATH=$(dirname "$INTO_MD_REAL_CL"):$PATH
    export PATH
    case "$INTO_MD_REAL_CL" in
      *"/$msvc_tools/"*) ;;
      *) echo "cl.exe is not from fixed MSVC tools $msvc_tools: $INTO_MD_REAL_CL" >&2; exit 2 ;;
    esac
    msvc_banner=$("$INTO_MD_REAL_CL" 2>&1 || true)
    INTO_MD_MSVC_BANNER_VERSION=$(printf '%s\n' "$msvc_banner" | sed -n 's/.*\([0-9][0-9]\.[0-9][0-9]\.[0-9][0-9]*\).*/\1/p' | head -n 1)
    test -n "$INTO_MD_MSVC_BANNER_VERSION" \
      || fail "could not determine the MSVC compiler version from cl.exe"
    export INTO_MD_MSVC_BANNER_VERSION
    msvc_cc_adapter="$root/tools/msvc-cl-adapter.sh"
    test -x "$msvc_cc_adapter" || fail "MSVC compiler adapter is missing or not executable"
    ;;
  *) echo "unsupported audit host" >&2; exit 2 ;;
esac

prefix=/opt/into-markdown/ffmpeg
stage="$work/stage"
set -- \
  --prefix="$prefix" --disable-everything --disable-gpl --disable-version3 --disable-nonfree \
  --disable-network --disable-autodetect --disable-programs --enable-ffmpeg --disable-ffprobe \
  --disable-doc --disable-debug --disable-devices --disable-avdevice \
  --disable-swscale --enable-avutil --enable-avcodec --enable-avformat --enable-avfilter \
  --enable-swresample --enable-protocol=file,pipe \
  --enable-demuxer=aac,avi,flac,matroska,mov,mp3,mpegts,ogg,wav \
  --enable-decoder=aac,flac,mp3,opus,vorbis,pcm_s8,pcm_s16be,pcm_s16le,pcm_s24be,pcm_s24le,pcm_s32be,pcm_s32le,pcm_f32be,pcm_f32le,pcm_f64be,pcm_f64le \
  --enable-parser=aac,mpegaudio,opus,vorbis --enable-filter=aformat,aresample,asetpts \
  --enable-encoder=pcm_s16le --enable-muxer=pcm_s16le --enable-static --disable-shared $toolchain_args
source_date_epoch=$(jq -er .source_date_epoch "$source_manifest")
actual_config=$(printf '%s\n' "$@" | sort)
expected_config=$({ printf '%s\n' "--prefix=$prefix"; jq -er --arg target "$target" '.required_flags[], .targets[$target].additional_flags[]' "$build_policy"; } | sort)
test "$actual_config" = "$expected_config"
(test "$format" = "$(jq -er --arg target "$target" '.targets[$target].binary_format' "$build_policy")")
(test "$arch" = "$(jq -er --arg target "$target" '.targets[$target].binary_architecture' "$build_policy")")
if [ "$format" = pe ]; then
  configure_command="env cc=$msvc_cc_adapter"
else
  configure_command=env
fi
if ! (cd "$src" && SOURCE_DATE_EPOCH="$source_date_epoch" $configure_command ./configure "$@"); then
  echo "FFmpeg configure failed; final config.log diagnostics:" >&2
  tail -n 200 "$src/ffbuild/config.log" >&2 || true
  exit 1
fi
(cd "$src" && SOURCE_DATE_EPOCH="$source_date_epoch" make -j2 && make DESTDIR="$stage" install)
if [ "$format" = pe ]; then
  tool="$stage$prefix/bin/ffmpeg.exe"
  artifact_name="ffmpeg-$target.exe"
else
  tool="$stage$prefix/bin/ffmpeg"
  artifact_name="ffmpeg-$target"
fi
test -x "$tool"
report="$work/version.txt"
"$tool" -hide_banner -version > "$report"
grep -q "^ffmpeg version $version" "$report"
grep -q -- '--disable-gpl' "$report"
grep -q -- '--disable-nonfree' "$report"
! grep -q -- '--enable-gpl' "$report"
! grep -q -- '--enable-nonfree' "$report"
for component in aac flac mp3 vorbis opus; do
  "$tool" -hide_banner -decoders 2>/dev/null | awk -v name="$component" '$1 ~ /^A/ && $2 == name {found=1} END {exit !found}'
done
for demuxer in wav mp3 mov flac ogg matroska; do
  "$tool" -hide_banner -demuxers 2>/dev/null | awk '$1 ~ /^D/ {print $2}' | tr ',' '\n' | grep -x "$demuxer" >/dev/null
done
if "$tool" -hide_banner -protocols 2>/dev/null | grep -E '^[[:space:]]+(http|https|tcp|udp)$'; then
  echo "network protocol leaked into FFmpeg build" >&2; exit 1
fi

# Deterministic real WAV smoke and malformed/non-audio failures. Codec fixtures
# are transient network inputs and are never copied to the artifact directory.
python3 - "$work/tone.wav" <<'PY'
import math, struct, sys, wave
with wave.open(sys.argv[1], "wb") as f:
    f.setnchannels(1); f.setsampwidth(2); f.setframerate(8000)
    f.writeframes(b"".join(struct.pack("<h", int(12000*math.sin(2*math.pi*440*n/8000))) for n in range(800)))
PY
"$tool" -nostdin -v error -protocol_whitelist file -i "$work/tone.wav" -ar 16000 -ac 1 -c:a pcm_s16le -f s16le "$work/tone.pcm"
test "$(wc -c < "$work/tone.pcm" | tr -d ' ')" = 3200
tab=$(printf '\t')
jq -e '.distribution == "transient-manual-ci-only" and .redistribution == "prohibited-license-unverified" and .included_in_artifacts == false' "$root/third_party/ffmpeg/fixtures.json" >/dev/null
jq -er '.fixtures[] | [.format,.url,(.bytes|tostring),.sha256] | @tsv' "$root/third_party/ffmpeg/fixtures.json" |
while IFS="$tab" read -r fixture_format fixture_url fixture_bytes fixture_sha; do
  fixture="$work/fixture.$fixture_format"
  download 1048576 "$fixture_url" "$fixture"
  test "$(wc -c < "$fixture" | tr -d ' ')" = "$fixture_bytes"
  printf '%s  %s\n' "$fixture_sha" "$fixture" | shasum -a 256 -c -
  "$tool" -nostdin -v error -protocol_whitelist pipe -i pipe:0 -af asetpts=N/SR/TB -frames:a 16000 \
    -ar 16000 -ac 1 -c:a pcm_s16le -f s16le pipe:1 < "$fixture" > "$work/$fixture_format.pcm"
  test -s "$work/$fixture_format.pcm"
done
if printf 'not media' | "$tool" -nostdin -v error -protocol_whitelist pipe -i pipe:0 -f s16le - >/dev/null 2>&1; then exit 1; fi

bytes=$(wc -c < "$tool" | tr -d ' ')
sha=$(shasum -a 256 "$tool" | awk '{print $1}')
if [ "$format" = pe ]; then compiler=$("$INTO_MD_REAL_CL" 2>&1 | head -n 1 || true); else compiler=$(cc --version 2>/dev/null | head -n 1 || true); fi
config_log_sha=$(shasum -a 256 "$src/ffbuild/config.log" | awk '{print $1}')
relink="$output_dir/ffmpeg-relink-$target.tar"
(cd "$src" && find . -type f \( -name '*.o' -o -path './ffbuild/config.log' -o -name 'config.h' -o -name 'Makefile' \) -print | LC_ALL=C sort > "$work/relink-files.txt")
test -s "$work/relink-files.txt"
tar -cf "$relink" -C "$src" -T "$work/relink-files.txt"
relink_bytes=$(wc -c < "$relink" | tr -d ' ')
relink_sha=$(shasum -a 256 "$relink" | awk '{print $1}')
policy_sha=$(shasum -a 256 "$build_policy" | awk '{print $1}')
deps='[]'
case "$format" in
  mach-o) deps=$(otool -L "$tool" | tail -n +2 | awk '{print $1}' | LC_ALL=C sort -u | jq -Rsc 'split("\n")[:-1]') ;;
  elf) deps=$(readelf -d "$tool" | awk '/NEEDED/ {gsub(/\[|\]/,"",$5); print $5}' | LC_ALL=C sort -u | jq -Rsc 'split("\n")[:-1]') ;;
  pe) deps=$(objdump -p "$tool" | awk '$1 == "DLL" && $2 == "Name:" {print $3}' | LC_ALL=C sort -u | jq -Rsc 'split("\n")[:-1]') ;;
esac
expected_deps=$(jq -cS --arg target "$target" '.targets[$target].dynamic_dependencies' "$build_policy")
actual_deps_sorted=$(printf '%s' "$deps" | jq -cS .)
expected_deps_sorted=$(printf '%s' "$expected_deps" | jq -cS .)
if [ "$actual_deps_sorted" != "$expected_deps_sorted" ]; then
  echo "FFmpeg dependency audit failed for $target" >&2
  echo "actual:   $actual_deps_sorted" >&2
  echo "expected: $expected_deps_sorted" >&2
  exit 1
fi
case "$format-$arch" in
  mach-o-aarch64) file "$tool" | grep -q 'Mach-O 64-bit executable arm64' ;;
  elf-x86_64) file "$tool" | grep -q 'ELF 64-bit.*x86-64' ;;
  elf-aarch64) file "$tool" | grep -q 'ELF 64-bit.*ARM aarch64' ;;
  pe-x86_64) file "$tool" | grep -Eq 'PE32\+ executable.*x86-64' ;;
esac
configure=$(printf '%s\n' "$@" | jq -Rsc 'split("\n")[:-1]')
jq -n --arg version "$version" --arg target "$target" --arg sha "$sha" --argjson bytes "$bytes" \
  --arg format "$format" --arg arch "$arch" --arg compiler "$compiler" --argjson configure "$configure" --argjson deps "$deps" \
  --arg source_sha "$source_sha" --arg signature_sha "$sig_sha" --arg fingerprint FCF986EA15E6E293A5644F10B4322F04D67658D8 \
  --arg policy_sha "$policy_sha" --arg config_log_sha "$config_log_sha" --arg relink_sha "$relink_sha" --argjson relink_bytes "$relink_bytes" \
  '{schema_version:1,ffmpeg_version:$version,target:$target,executable_bytes:$bytes,executable_sha256:$sha,configure:$configure,binary_format:$format,binary_architecture:$arch,dependencies:$deps,toolchain:$compiler,source_sha256:$source_sha,source_signature_sha256:$signature_sha,signing_key_fingerprint:$fingerprint,build_policy_sha256:$policy_sha,config_log_sha256:$config_log_sha,relink_bytes:$relink_bytes,relink_sha256:$relink_sha}' > "$output_dir/ffmpeg-authority-$target.json"
cp "$tool" "$output_dir/$artifact_name"
cp "$src/COPYING.LGPLv2.1" "$output_dir/COPYING.LGPLv2.1"
license_sha=$(shasum -a 256 "$output_dir/COPYING.LGPLv2.1" | awk '{print $1}')
jq -n --arg target "$target" --arg binary "$artifact_name" \
  --arg authority "ffmpeg-authority-$target.json" --arg license_sha "$license_sha" \
  --arg relink "ffmpeg-relink-$target.tar" \
  '{schema_version:1,target:$target,distributed_files:[$binary,$authority,"COPYING.LGPLv2.1",$relink],license_sha256:$license_sha,fixture_policy:{included:false,usage:"transient manual CI decoder smoke",redistribution:"prohibited-license-unverified"}}' \
  > "$output_dir/ffmpeg-inventory-$target.json"
artifact_sha_before=$(shasum -a 256 "$output_dir/$artifact_name" | awk '{print $1}')
artifact_bytes_before=$(wc -c < "$output_dir/$artifact_name" | tr -d ' ')
if [ "${FFMPEG_AUDIT_PRODUCTION_SMOKE:-}" = 1 ]; then
  fixture_dir="$work/production-fixtures"; mkdir "$fixture_dir"
  jq -er '.fixtures[] | [.format,.url,.sha256] | @tsv' "$root/third_party/ffmpeg/fixtures.json" |
  while IFS="$tab" read -r fixture_format fixture_url fixture_sha; do
    download 1048576 "$fixture_url" "$fixture_dir/sample.$fixture_format"
    printf '%s  %s\n' "$fixture_sha" "$fixture_dir/sample.$fixture_format" | shasum -a 256 -c -
  done
  test_executable=$(cd "$output_dir" && pwd)/$artifact_name
  test_authority=$(cd "$output_dir" && pwd)/ffmpeg-authority-$target.json
  test_fixtures=$fixture_dir
  if [ "$format" = pe ]; then
    test_executable=$(cygpath -w "$test_executable")
    test_authority=$(cygpath -w "$test_authority")
    test_fixtures=$(cygpath -w "$test_fixtures")
  fi
  FFMPEG_TEST_EXECUTABLE="$test_executable" \
    FFMPEG_TEST_AUTHORITY="$test_authority" \
    FFMPEG_TEST_FIXTURES="$test_fixtures" cargo test -p into-markdown-ffmpeg native_smoke -- --ignored
fi
test "$(shasum -a 256 "$output_dir/$artifact_name" | awk '{print $1}')" = "$artifact_sha_before"
test "$(wc -c < "$output_dir/$artifact_name" | tr -d ' ')" = "$artifact_bytes_before"
expected_files=$(printf '%s\n' "COPYING.LGPLv2.1" "$artifact_name" "ffmpeg-authority-$target.json" "ffmpeg-inventory-$target.json" "ffmpeg-relink-$target.tar" | sort)
actual_files=$(find "$output_dir" -mindepth 1 -maxdepth 1 -type f -exec basename {} \; | sort)
test "$actual_files" = "$expected_files"
test "$(find "$output_dir" -mindepth 1 -maxdepth 1 ! -type f -print -quit)" = ""
echo "FFmpeg audit passed: $target $bytes bytes $sha"
