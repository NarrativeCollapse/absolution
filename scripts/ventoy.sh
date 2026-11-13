#!/bin/sh

# Build the live images and copy them to a Ventoy-formatted device

set -e

ARCHS="i586 x86_64"
CONFIGS="minimal server"

VENTOY="/media/${USER}/Ventoy"
if [ ! -d "${VENTOY}" ]
then
    echo "Ventoy not mounted" >&2
    exit 1
fi

for ARCH in $ARCHS
do
    for CONFIG_NAME in $CONFIGS
    do
        IMAGE="build/${ARCH}/${CONFIG_NAME}/absolution-live.iso"
        DEST="${VENTOY}/absolution-${CONFIG_NAME}-${ARCH}.iso"
        make ARCH="${ARCH}" CONFIG_NAME="${CONFIG_NAME}" "${IMAGE}"
        echo "Copying ${IMAGE} to ${DEST}"
        cp "${IMAGE}" "${DEST}"
    done
done

sync

echo "Finished copying configs (${CONFIGS}) for archs (${ARCHS})"
