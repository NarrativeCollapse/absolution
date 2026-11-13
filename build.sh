#!/bin/sh

# Alternative to setting ARCH, CONFIG_NAME and FILESYSTEM_CONFIG for make

usage()
{
    cat <<'USAGE'
build.sh:      Invoke make for a particular architecture and configuration.
Usage:
./build.sh [-X | -A | -5 | -R | -a ARCH] [-c CONFIG] [-f FILESYSTEM_CONFIG] TARGET...
    -X         Equivalent to -a x86_64.
    -A         Equivalent to -a aarch64.
    -5         Equivalent to -a i586.
    -6         Equivalent to -a i586 (deprecated, use -5 instead).
    -R         Equivalent to -a riscv64gc.
    -a ARCH:   Processor Architecture. Normally one of x86_64, aarch64,
               riscv64gc or i586. ARCH is not checked, so you can add a new
               architecture. Defaults to the directory containing the
               FILESYSTEM_CONFIG file, or x86_64 if no FILESYSTEM_CONFIG is
               specified.
    -c CONFIG: The name of the config, e.g. server or minimal.
               Determines the name of the image, build/ARCH/CONFIG/harddrive.img
               e.g. build/x86_64/server/harddrive.img
               Determines the name of FILESYSTEM_CONFIG if none is specified.
               Defaults to the basename of FILESYSTEM_CONFIG, or 'server'
               if FILESYSTEM_CONFIG is not specified.
    -f FILESYSTEM_CONFIG:
               The config file to use. It can be in any location.
               However, if the file is not in a directory named after an
               architecture, you must specify the architecture.
               If -f is not specified, FILESYSTEM_CONFIG is set to
               config/ARCH/CONFIG.toml
    Examples:  ./build.sh -c minimal live - make build/x86_64/minimal/absolution-live.iso
               ./build.sh -5 qemu - make build/i586/server/harddrive.img and
                                    run it in qemu
    NOTE:      If you do not change ARCH or CONFIG very often, set them in
               .config instead. You only need this script to override them.
USAGE
}

if [ "$1" = "-h" ] || [ "$1" = "--help" ]; then
    usage
    exit 0
fi

ARCH=""
CONFIG_NAME=""
FILESYSTEM_CONFIG=""

while getopts ":c:f:a:hXAR56" opt
do
    case "$opt" in
        a) ARCH="$OPTARG";;
        c) CONFIG_NAME="$OPTARG";;
        f) FILESYSTEM_CONFIG="$OPTARG";;
        X) ARCH="x86_64";;
        A) ARCH="aarch64";;
        R) ARCH="riscv64gc";;
        5|6) ARCH="i586";;
        h) usage; exit 0;;
        :) echo "-$OPTARG requires a value" >&2; exit 1;;
        *) echo "Unknown option -$OPTARG, try -h for help" >&2; exit 1;;
    esac
done
shift $((OPTIND - 1))

if [ -z "$ARCH" ] && [ -n "$FILESYSTEM_CONFIG" ]; then
    ARCH=$(basename "$(dirname "$FILESYSTEM_CONFIG")")
    case "$ARCH" in
        x86_64|aarch64|riscv64gc|i586) ;;
        *)
            echo "Cannot tell the architecture from $FILESYSTEM_CONFIG, please specify it with -a" >&2
            exit 1
            ;;
    esac
fi

if [ -z "$CONFIG_NAME" ] && [ -n "$FILESYSTEM_CONFIG" ]; then
    CONFIG_NAME=$(basename "$FILESYSTEM_CONFIG" .toml)
fi

ARCH="${ARCH:-x86_64}"
CONFIG_NAME="${CONFIG_NAME:-server}"
FILESYSTEM_CONFIG="${FILESYSTEM_CONFIG:-config/$ARCH/$CONFIG_NAME.toml}"

export ARCH CONFIG_NAME FILESYSTEM_CONFIG
exec make "$@"
