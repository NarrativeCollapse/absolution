#!/bin/sh

# Show every recipe file that contains the given text (needs ripgrep and bat)

if [ -z "$1" ]
then
    echo "Usage: $0 <text>" >&2
    exit 1
fi

files="$(rg -li --sort=path -- "$1" recipes)" || exit 1
# One file per line; split on newlines only and do not expand globs.
IFS='
'
set -f
# shellcheck disable=SC2086
bat --decorations=always $files
