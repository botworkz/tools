#!/bin/sh
set -eu

uid="$(id -u)"
gid="$(id -g)"

if ! getent passwd "$uid" >/dev/null 2>&1; then
    home_dir="${HOME:-/tmp}"
    if [ ! -d "$home_dir" ] || [ ! -w "$home_dir" ]; then
        home_dir="/tmp"
    fi
    export HOME="$home_dir"

    NSS_PASSWD="$(mktemp)"
    NSS_GROUP="$(mktemp)"
    export NSS_WRAPPER_PASSWD="$NSS_PASSWD"
    export NSS_WRAPPER_GROUP="$NSS_GROUP"

    printf 'botforge:x:%s:%s:botforge:%s:/bin/sh\n' "$uid" "$gid" "$home_dir" >"$NSS_PASSWD"
    printf 'botforge:x:%s:\n' "$gid" >"$NSS_GROUP"

    nss_wrapper_lib=""
    for candidate in \
        /usr/lib/*/libnss_wrapper.so \
        /usr/lib/libnss_wrapper.so \
        /lib/*/libnss_wrapper.so \
        /lib/libnss_wrapper.so
    do
        if [ -f "$candidate" ]; then
            nss_wrapper_lib="$candidate"
            break
        fi
    done

    if [ -z "$nss_wrapper_lib" ]; then
        echo "error: libnss_wrapper.so not found" >&2
        exit 1
    fi

    if [ -n "${LD_PRELOAD:-}" ]; then
        export LD_PRELOAD="$nss_wrapper_lib:$LD_PRELOAD"
    else
        export LD_PRELOAD="$nss_wrapper_lib"
    fi
fi

exec botforge "$@"
