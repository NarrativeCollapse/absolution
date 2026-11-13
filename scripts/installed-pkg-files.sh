#!/bin/sh

# Show which recipes installed each file of the image

# The architecture and config select the build directory.
ARCH=$(uname -m)
CONFIG_NAME=server
export ARCH CONFIG_NAME

# Make sure the image is not mounted from an earlier run
make unmount >/dev/null 2>&1 || true

make mount >/dev/null

find "build/${ARCH}/${CONFIG_NAME}/" -type f | cut -d / -f5- | sort -u |
while IFS= read -r path
do
    if [ -z "${path}" ]
    then
        continue
    fi

    # Every recipe whose stage provides this file
    pkgs="$(
        for stage_file in recipes/*/*/"target/${ARCH}-unknown-redox/stage/${path}"
        do
            [ -e "$stage_file" ] && printf '%s\n' "$stage_file" | cut -d / -f3
        done | sort -u | tr '\n' ' '
    )"
    if [ -n "${pkgs}" ]
    then
        echo "$path: ${pkgs}"
    else
        echo "$path: no packages, see config/${ARCH}/${CONFIG_NAME}.toml"
    fi
done

make unmount >/dev/null 2>&1 || true
