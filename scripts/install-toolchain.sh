#!/bin/sh
set -eu
export LC_ALL=C

releases=https://github.com/zkcl-dev/zkcl/releases
text_asset_limit=256
archive_limit=$((256 * 1024 * 1024))
extraction_limit=$((512 * 1024 * 1024))

usage() {
    cat <<'EOF'
Usage: install-toolchain.sh [--toolchain SELECTOR] [--prefix PATH]
                            [--from DIRECTORY|HTTPS_URL]

Install zkcl and zkclx for this host. Select stable (the default), beta,
nightly, a version such as 1.2.3, or a dated nightly such as nightly-2026-11-23.
Use --from to install from a local distribution directory or HTTPS mirror.
EOF
}

fail() {
    printf 'zkcl installer: %s\n' "$*" >&2
    exit 1
}

host_target() {
    case "$(uname -s):$(uname -m)" in
        Darwin:arm64) printf '%s\n' aarch64-apple-darwin ;;
        Darwin:x86_64) printf '%s\n' x86_64-apple-darwin ;;
        Linux:aarch64) printf '%s\n' aarch64-unknown-linux-gnu ;;
        Linux:x86_64) printf '%s\n' x86_64-unknown-linux-gnu ;;
        *) fail 'Unsupported zkcl toolchain host.' ;;
    esac
}

absolute_path() {
    case "$1" in
        /*) path=$1 ;;
        *)
            # Keep any trailing newlines in the working directory name.
            cwd=$(pwd -P && printf '.') || fail 'Could not find the working directory.'
            cwd=${cwd%??}
            path="${cwd%/}/$1"
            ;;
    esac
}

limit_file_size() (
    # Also bound writes when an older curl cannot check a chunked response size.
    blocks=$(( ($1 + 511) / 512 ))
    shift
    current=$(ulimit -f) || exit 1
    if [ "$current" = unlimited ] || [ "$current" -gt "$blocks" ]; then
        ulimit -f "$blocks" || exit 1
    fi
    exec "$@"
)

fetch_asset() {
    asset="${source%/}/$1"
    case "$source" in
        https://*)
            limit_file_size "$3" curl --disable --fail --silent --show-error --location --globoff \
                --proto '=https' --proto-redir '=https' --tlsv1.2 \
                --connect-timeout 15 --retry 2 --max-time 300 --max-filesize "$3" \
                --output "$2" -- "$asset" || fail "Could not download '$1'."
            ;;
        *)
            [ -f "$asset" ] || fail "Release asset '$asset' must be a regular file."
            head -c "$(( $3 + 1 ))" "$asset" > "$2" || fail "Could not read '$asset'."
            ;;
    esac
    bytes=$(wc -c < "$2")
    [ "$bytes" -le "$3" ] || fail "Release asset '$1' exceeds $3 bytes."
}

read_line() {
    if ! IFS= read -r line < "$1" || ! printf '%s\n' "$line" | cmp -s - "$1"; then
        fail "Expected a single LF-terminated line in '${1##*/}'."
    fi
    printf '%s\n' "$line"
}

release_tag() {
    case "$1" in
        ''|*[!A-Za-z0-9.+-]*) fail "Invalid toolchain release '$1'." ;;
    esac
    [ "${#1}" -le 96 ] || fail 'Toolchain release ID exceeds 96 bytes.'

    if ! printf '%s\n' "$1" | awk '
        function valid_date(year, month, day, limit) {
            if (year < 1 || month < 1 || month > 12 || day < 1) return 0
            limit = 31
            if (month == 4 || month == 6 || month == 9 || month == 11) limit = 30
            if (month == 2) {
                limit = 28 + (year % 4 == 0 && (year % 100 != 0 || year % 400 == 0))
            }
            return day <= limit
        }
        function valid_version(version, core, parts, component, count, i) {
            sub(/[+].*$/, "", version)
            core = version
            sub(/-.*/, "", core)
            split(core, parts, "[.]")
            for (i = 1; i <= 3; i++) {
                component = parts[i]
                if (component ~ /^0[0-9]/ || length(component) > 20) return 0
                # Compare decimal u64 components without converting them to floats.
                if (length(component) == 20 && "n" component > "n18446744073709551615") return 0
            }
            if (version != core) {
                count = split(substr(version, length(core) + 2), parts, "[.]")
                for (i = 1; i <= count; i++) {
                    if (parts[i] ~ /^0[0-9]+$/) return 0
                }
            }
            return 1
        }
        /^nightly-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]$/ {
            valid = valid_date(substr($0, 9, 4) + 0, substr($0, 14, 2) + 0, substr($0, 17, 2) + 0)
        }
        /^[0-9]+[.][0-9]+[.][0-9]+(-[A-Za-z0-9-]+([.][A-Za-z0-9-]+)*)?([+][A-Za-z0-9-]+([.][A-Za-z0-9-]+)*)?$/ {
            valid = valid_version($0)
        }
        END { exit !valid }
    '; then
        fail "Invalid toolchain release '$1'."
    fi
    case "$1" in
        nightly-*) printf '%s\n' "$1" ;;
        *) printf 'v%s\n' "$1" ;;
    esac
}

fetch_launcher() {
    bundle="zkcl-launcher-$host"
    archive="$bundle.tar.gz"
    fetch_asset "download/$tag/$archive.sha256" "$stage/$archive.sha256" "$text_asset_limit"
    checksum_line=$(read_line "$stage/$archive.sha256")
    checksum=${checksum_line%% *}
    case "${checksum_line#"$checksum"}" in
        "  $archive"|" *$archive") ;;
        *) fail "Invalid checksum record for '$archive'." ;;
    esac
    case "$checksum" in
        *[!0-9A-Fa-f]*) fail "Invalid SHA256 checksum for '$archive'." ;;
    esac
    [ "${#checksum}" -eq 64 ] || fail "Invalid SHA256 checksum for '$archive'."
    checksum=$(printf '%s' "$checksum" | tr 'A-F' 'a-f')

    fetch_asset "download/$tag/$archive" "$stage/$archive" "$archive_limit"
    case "$host" in
        *-apple-darwin) actual=$(shasum -a 256 < "$stage/$archive") ;;
        *-unknown-linux-gnu) actual=$(sha256sum < "$stage/$archive") ;;
    esac || fail "Could not hash '$archive'."
    [ "${actual%% *}" = "$checksum" ] || fail "SHA256 mismatch for '$archive'."

    # Stream just the executable so the archive cannot write files or links.
    TAR_OPTIONS= limit_file_size "$extraction_limit" \
        tar -xOzf "$stage/$archive" "$bundle/bin/zkcl" > "$stage/zkcl" ||
        fail "Could not extract zkcl from '$archive'."
    [ -s "$stage/zkcl" ] || fail "No zkcl executable in '$archive'."
    chmod 0755 "$stage/zkcl"
}

main() {
    selector=stable
    source=$releases
    prefix=
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -h|--help) usage; return 0 ;;
            --toolchain|--from|--prefix)
                [ "$#" -ge 2 ] && [ -n "$2" ] || fail "Missing value for $1."
                case "$1" in
                    --toolchain) selector=$2 ;;
                    --from) source=$2 ;;
                    --prefix) prefix=$2 ;;
                esac
                shift 2
                ;;
            *) fail "Unknown option '$1'. See --help." ;;
        esac
    done

    case "$source" in
        https://*)
            url_tail=${source#https://}
            case "${url_tail%%/*}" in
                ''|*@*) fail '--from requires an HTTPS distribution root without credentials.' ;;
            esac
            case "$url_tail" in
                *[?#]*) fail '--from requires an HTTPS distribution root without a query or fragment.' ;;
            esac
            while [ "${source%/}" != "$source" ]; do
                source=${source%/}
            done
            ;;
        /*) ;;
        *://*) fail '--from requires an HTTPS URL or a local directory.' ;;
        *) absolute_path "$source"; source=$path ;;
    esac
    prefix=${prefix:-${ZKCL_HOME:-${XDG_DATA_HOME:-${HOME:?Set HOME or pass --prefix}/.local/share}/zkcl}}
    absolute_path "$prefix"
    prefix=$path

    pointer=
    case "$selector" in
        stable) pointer=latest/download/zkcl-channel-v1.txt ;;
        beta|nightly) pointer="download/$selector/zkcl-channel-v1.txt" ;;
        *) exact=$selector; tag=$(release_tag "$exact") ;;
    esac

    host=$(host_target)
    absolute_path "${TMPDIR:-/tmp}"
    stage=
    trap 'if [ -n "$stage" ]; then rm -rf "$stage"; fi' 0
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
    stage=$(mktemp -d "${path%/}/zkcl-install.XXXXXX") ||
        fail 'Could not create a temporary installer directory.'

    if [ -n "$pointer" ]; then
        fetch_asset "$pointer" "$stage/zkcl-channel-v1.txt" "$text_asset_limit"
        exact=$(read_line "$stage/zkcl-channel-v1.txt")
        tag=$(release_tag "$exact")
    fi
    fetch_launcher
    "$stage/zkcl" __bootstrap --prefix "$prefix" --from "$source" \
        --toolchain "$selector" --resolved "$exact"

    quoted_bin=$(printf '%s\n' "${prefix%/}/bin" | sed "s/'/'\\\\''/g")
    printf "Add zkcl and zkclx to PATH with\n  export PATH='%s'\${PATH:+:\"\$PATH\"}\n" "$quoted_bin"
}

main "$@"
