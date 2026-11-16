#!/bin/sh
set -eu
export LC_ALL=C

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ] || [ -z "$1" ]; then
    echo 'Usage: package-toolchain.sh <output-directory> [<exact-toolchain>]' >&2
    exit 2
fi

fail() {
    printf 'zkcl packaging: %s\n' "$*" >&2
    exit 1
}

write_sdk_manifest() {
    link_args=$(sed -n 's/^.*native-static-libs: //p' "$link_report" | tail -n 1)
    [ -n "$link_args" ] || fail 'rustc did not report native-static-libs.'
    frameworks='[]'
    case "$target" in
        *-apple-darwin) frameworks='["-framework","Security","-framework","CoreFoundation"]' ;;
    esac
    # rustc does not include framework flags in its native-static-libs report.
    jq -n --arg version "$version" --arg target "$target" --arg flags "$link_args" \
        --argjson frameworks "$frameworks" \
        '{version:$version,target:$target,link_args:([$flags | scan("\\S+")] + $frameworks)}' \
        > "$sdk_directory/runtime.json"
}

case "$(uname -s):$(uname -m)" in
    Darwin:arm64) target=aarch64-apple-darwin ;;
    Darwin:x86_64) target=x86_64-apple-darwin ;;
    Linux:aarch64) target=aarch64-unknown-linux-gnu ;;
    Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
    *) fail 'Unsupported toolchain host.' ;;
esac

command -v jq >/dev/null || fail 'Packaging requires jq.'

output=$1
case "$output" in
    /*) ;;
    *) output="$PWD/$output" ;;
esac
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"

commit=$(git rev-parse HEAD)
case "$commit" in *[!0-9a-f]*) fail 'Expected a hexadecimal Git commit.' ;; esac
[ "${#commit}" -eq 40 ] || fail 'Expected a 40-character Git commit.'

mkdir -p "$output"
link_report=$(mktemp "$output/.native-libs.XXXXXX")
trap 'rm -f "$link_report"' 0
target_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked --release --package zkcl-cli --package zkcl-toolchain \
    --bin zkcl --bin zkcl-launcher \
    --target "$target" --target-dir "$target_dir"

CARGO_TERM_COLOR=never cargo rustc --locked --release --package zkcl-capi --lib \
    --target "$target" --target-dir "$target_dir" -- --print native-static-libs 2> "$link_report" || { cat "$link_report" >&2; exit 1; }
cat "$link_report" >&2

if [ "${ZKCL_PACKAGE_WASM:-0}" = 1 ]; then
    cargo build --locked --release --package zkcl-runtime --example wasm_runtime \
        --target wasm32-unknown-unknown --target-dir "$target_dir"
fi

bin="$target_dir/$target/release"
report=$("$bin/zkcl" --version)
case "$report" in
    'zkcl '*) version=${report#zkcl } ;;
    *) fail "Could not read the version from '$report'." ;;
esac
case "$version" in
    ''|[!0-9]*|*[!A-Za-z0-9.+-]*) fail "Could not read the version from '$report'." ;;
esac
[ "${#version}" -le 96 ] || fail "Version '$version' exceeds 96 characters."

toolchain=${2:-$version}
case "$toolchain" in
    "$version") ;;
    nightly-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9])
        day=${toolchain#nightly-}
        case "$target" in
            *-apple-darwin) normalized=$(date -j -u -f '%Y-%m-%dT%H:%M:%S' "${day}T00:00:00" +%Y-%m-%d) ;;
            *) normalized=$(date -u -d "$day" +%Y-%m-%d) ;;
        esac
        [ "$normalized" = "$day" ] || fail "Invalid nightly date '$day'."
        ;;
    *) fail "Expected exact toolchain '$version' or a dated nightly, received '$toolchain'." ;;
esac

runtime="zkcl-$toolchain-$target"
launcher="zkcl-launcher-$target"
sdk="zkcl-sdk-$toolchain-$target"
wasm_sdk="zkcl-sdk-$toolchain-wasm32-unknown-unknown"
manifest="zkcl-toolchain-$target.json"
mkdir -p "$output"
stage=$(mktemp -d "$output/.zkcl-package.XXXXXX")
trap 'rm -rf "$stage"; rm -f "$link_report"' 0
trap 'exit 1' HUP INT TERM

mkdir -p "$stage/$runtime/bin" "$stage/$launcher/bin"
install -m 0644 LICENSE "$stage/$runtime/LICENSE"
install -m 0644 LICENSE "$stage/$launcher/LICENSE"
install -m 0644 crates/zkcl-runtime/lisp/bundled/asdf/LICENSE "$stage/$runtime/LICENSE-ASDF"
install -m 0755 "$bin/zkcl" "$stage/$runtime/bin/zkcl"
install -m 0755 "$bin/zkcl-launcher" "$stage/$launcher/bin/zkcl"

sdk_directory="$stage/$runtime/sdk/$target"
mkdir -p "$sdk_directory/lib" "$sdk_directory/include"
install -m 0644 "$bin/libzkcl_capi.a" "$sdk_directory/lib/"
install -m 0644 crates/zkcl-capi/include/zkcl.h "$sdk_directory/include/"
write_sdk_manifest
mkdir -p "$stage/$sdk/sdk"
cp -R "$sdk_directory" "$stage/$sdk/sdk/$target"
if [ "${ZKCL_PACKAGE_WASM:-0}" = 1 ]; then
    wasm_directory="$stage/$runtime/sdk/wasm32-unknown-unknown"
    mkdir -p "$wasm_directory" "$stage/$wasm_sdk/sdk"
    install -m 0644 "$target_dir/wasm32-unknown-unknown/release/examples/wasm_runtime.wasm" "$wasm_directory/runtime.wasm"
    printf '{"kind":"wasm","version":"%s","target":"wasm32-unknown-unknown"}\n' "$version" > "$wasm_directory/runtime.json"
    cp -R "$wasm_directory" "$stage/$wasm_sdk/sdk/wasm32-unknown-unknown"
fi

case "$target" in
    *-apple-darwin) tar_format=gnutar ;;
    *) tar_format=gnu ;;
esac

hash() {
    case "$target" in
        *-apple-darwin) shasum -a 256 "$1" ;;
        *) sha256sum "$1" ;;
    esac
}

(
    cd "$stage"
    COPYFILE_DISABLE=1 tar --format "$tar_format" -czf "$runtime.tar.gz" "$runtime"
    COPYFILE_DISABLE=1 tar --format "$tar_format" -czf "$launcher.tar.gz" "$launcher"
    COPYFILE_DISABLE=1 tar --format "$tar_format" -czf "$sdk.tar.gz" "$sdk"
    sdk_hash=$(hash "$sdk.tar.gz")
    sdk_size=$(wc -c < "$sdk.tar.gz" | tr -d '[:space:]')
    wasm_json=
    if [ "${ZKCL_PACKAGE_WASM:-0}" = 1 ]; then
        COPYFILE_DISABLE=1 tar --format "$tar_format" -czf "$wasm_sdk.tar.gz" "$wasm_sdk"
        wasm_hash=$(hash "$wasm_sdk.tar.gz")
        wasm_size=$(wc -c < "$wasm_sdk.tar.gz" | tr -d '[:space:]')
        wasm_json=", \"wasm32-unknown-unknown\": {\"size\": $wasm_size, \"sha256\": \"${wasm_hash%% *}\"}"
    fi
    runtime_hash=$(hash "$runtime.tar.gz")
    launcher_hash=$(hash "$launcher.tar.gz")
    runtime_size=$(wc -c < "$runtime.tar.gz" | tr -d '[:space:]')
    launcher_size=$(wc -c < "$launcher.tar.gz" | tr -d '[:space:]')

    printf '%s\n' "$launcher_hash" > "$launcher.tar.gz.sha256"
    cat > "$manifest" <<EOF
{
  "format": 1,
  "toolchain": "$toolchain",
  "version": "$version",
  "commit": "$commit",
  "targets": {
    "$target": {
      "runtime": {"size": $runtime_size, "sha256": "${runtime_hash%% *}"},
      "launcher": {"size": $launcher_size, "sha256": "${launcher_hash%% *}"},
      "sdk": {"$target": {"size": $sdk_size, "sha256": "${sdk_hash%% *}"}$wasm_json}
    }
  }
}
EOF
)

for file in "$stage/"*.tar.gz "$stage/"*.sha256 "$stage/$manifest"; do
    file=${file##*/}
    mv -f "$stage/$file" "$output/$file"
    printf '%s\n' "$output/$file"
done
