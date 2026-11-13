#!/bin/sh

# Print a recipe's configuration, or another file in its directory

if [ -z "$1" ]
then
    echo "Usage: $0 <recipe-name> [file]" >&2
    exit 1
fi

file=${2:-recipe.toml}
recipe_path=$(make "find.$1" | tr -d '\r')

cat "$recipe_path/$file"
