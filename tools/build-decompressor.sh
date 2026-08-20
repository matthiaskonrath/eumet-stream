#!/usr/bin/env bash
# Build EUMETSAT's xRITDecompress from source, for Linux.
#
# SEVIRI HRIT pixel data is wavelet-compressed, and the only implementation is
# EUMETSAT's own (Apache 2.0). It is C++, so it is built once and invoked as a
# helper. Everything else in this project is Rust.
#
#   tools/build-decompressor.sh
#
# The result is dropped in tools/xRITDecompress, where the server finds it
# automatically. Set XRIT_DECOMPRESS to override the location.
#
# Upstream's build only supports Windows, Linux and Solaris - see
# vendor/PublicDecompWT/conda/meta.yaml. There is no macOS build here; on
# macOS the NWC SAF layers still work, see the README.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$script_dir/.." && pwd)"
vendor="$root/vendor/PublicDecompWT"

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "error: upstream's build only supports Windows, Linux and Solaris (this is $(uname -s))." >&2
    echo "       see vendor/PublicDecompWT/conda/meta.yaml." >&2
    exit 1
fi

if ! command -v make >/dev/null 2>&1 || ! command -v g++ >/dev/null 2>&1; then
    echo "error: this needs 'make' and 'g++' - e.g. 'apt install build-essential'." >&2
    exit 1
fi

if [[ ! -d "$vendor" ]]; then
    echo "Cloning PublicDecompWT..."
    mkdir -p "$root/vendor"
    # LFS-tracked test data isn't needed to build; skip pulling it.
    GIT_LFS_SKIP_SMUDGE=1 git clone --depth 1 https://gitlab.eumetsat.int/open-source/PublicDecompWT.git "$vendor"
fi

# The Makefile's first (and so default) target is "uninstall", not "all" -
# without this, plain `make` silently removes any installed copy and exits
# 0 without building anything.
make -C "$vendor/xRITDecompress" all

built="$vendor/xRITDecompress/xRITDecompress"
if [[ ! -x "$built" ]]; then
    echo "error: build reported success but $built is missing." >&2
    exit 1
fi

cp "$built" "$script_dir/xRITDecompress"
echo
echo "Installed: $script_dir/xRITDecompress"
