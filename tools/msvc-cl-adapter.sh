#!/bin/sh
set -eu

# FFmpeg detects cl.exe by matching an English banner. VSLANG only changes the
# banner when the corresponding Visual Studio language resources are installed,
# which is not guaranteed on non-English Windows builders. Keep the compiler
# itself authoritative and adapt only the two banner probes.
: "${INTO_MD_REAL_CL:?INTO_MD_REAL_CL must name the fixed MSVC cl.exe}"
: "${INTO_MD_MSVC_BANNER_VERSION:?INTO_MD_MSVC_BANNER_VERSION must be pinned}"
: "${INTO_MD_MSVC_BANNER_ARCH:?INTO_MD_MSVC_BANNER_ARCH must be pinned}"
case "$INTO_MD_MSVC_BANNER_ARCH" in
  x64|ARM64) ;;
  *) echo "unsupported MSVC banner architecture: $INTO_MD_MSVC_BANNER_ARCH" >&2; exit 2 ;;
esac

banner_probe=false
if [ "$#" -eq 0 ]; then
  banner_probe=true
else
  for argument in "$@"; do
    if [ "$argument" = -nologo- ]; then
      banner_probe=true
      break
    fi
  done
fi

if [ "$banner_probe" = true ]; then
  printf 'Microsoft (R) C/C++ Optimizing Compiler Version %s for %s\n' \
    "$INTO_MD_MSVC_BANNER_VERSION" "$INTO_MD_MSVC_BANNER_ARCH"
  "$INTO_MD_REAL_CL" "$@"
  exit $?
fi

exec "$INTO_MD_REAL_CL" "$@"
