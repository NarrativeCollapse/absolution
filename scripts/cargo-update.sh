#!/bin/sh

# This script runs "make f.recipe" and "cargo update" in the specified recipe

set -e

if [ -z "$1" ]
then
    echo "Usage: $0 <recipe-name>" >&2
    exit 1
fi

recipe_name="$1"
recipe_path="$(target/release/repo find "$recipe_name")"

make "f.$recipe_name"
cd "$recipe_path/source"
cargo update
