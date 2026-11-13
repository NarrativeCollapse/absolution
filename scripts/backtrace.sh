#!/bin/sh

# This script allow the user to copy a Rust backtrace from Absolution
# and retrieve the symbols

usage()
{
    echo "Usage: $0 -r recipe [ -e command_name ] [ -R ] [ -X | -5 | -A ] [[ -b backtracefile ] | [ addr1 ... ]]"
    echo
    echo "Print the backtrace contained in the backtracefile."
    echo "Symbols are taken from the executable for the given recipe."
    echo "If no backtracefile is given, decode the given addresses instead."
    echo "This command must be run from the root of the build tree."
    echo
    echo "-X for x86_64, -5 for i586, -A for aarch64 (x86_64 is the default)."
    echo "To read from stdin, use '-b -'"
    echo "The name of the executable must match what Cargo believes it to be."
    echo "If the executalbe is named 'recipe_command', just use 'command' as the name."
    echo "The debug version of the executable is used if available."
    echo "The release version is used if no debug version exists."
    echo "-R to force the use of the 'release' version of the executable."
    echo "Make sure the executable is the one that produced the backtrace."
    exit 1
}

ARCH="x86_64"

while getopts ":b:e:r:hRXA56" opt
do
    case "$opt" in
        X) ARCH="x86_64";;
        A) ARCH="aarch64";;
        5|6) ARCH="i586";;
        b) INFILE="$OPTARG";;
        e) COMMAND="$OPTARG";;
        r) RECIPE_NAME="$OPTARG";;
        R) RELEASE=true;;
        h) usage;;
        :) echo "-$OPTARG requires a value" >&2; exit 1;;
        *) echo "Unknown option -$OPTARG, try -h for help" >&2; exit 1;;
    esac
done
shift $((OPTIND -1))

if [ -z "$RECIPE_NAME" ]
then
    usage
fi

if [ -z "$INFILE" ] && [ $# -eq 0 ]
then
    usage
fi

# if no command name is given, assume it's the same as the recipe name
RECIPE_DIR="$(target/release/repo find "$RECIPE_NAME")"
if [ -z "$COMMAND" ]
then
    COMMAND="$RECIPE_NAME"
fi

# look for the debug version of the command
EXECUTABLE="$RECIPE_DIR"/target/"$ARCH"-unknown-redox/build/target/"$ARCH"-unknown-redox/debug/"$COMMAND"

# try the release version next
if [ ! -f "$EXECUTABLE" ] || [ -n "$RELEASE" ]
then
    EXECUTABLE="$RECIPE_DIR"/target/"$ARCH"-unknown-redox/build/target/"$ARCH"-unknown-redox/release/"$COMMAND"
fi

if [ $# -ne 0 ]
then
    addr2line --demangle=rust --inlines --pretty-print --functions --exe="$EXECUTABLE" "$@"
else
    sed '/^[[:space:]]*$/d; s/^.*0x\([0-9a-f]*\).*$/\1/g' "$INFILE" | addr2line --demangle=rust --inlines --pretty-print --functions --exe="$EXECUTABLE"
fi

