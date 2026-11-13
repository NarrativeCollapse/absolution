#!/bin/sh

set -e

# Open a recipe file with $EDITOR, or with xdg-open.
# Run this outside podman.

if [ -z "$1" ]
then
    echo "Open a file into a recipe."
    echo ""
    echo "Usage: [EDITOR=program] $0 <recipe-name> [file]"
    echo "  [EDITOR=program]: The program to open the file with, default 'xdg-open'"
    echo "  <recipe-name>: The recipe name, cannot be multiple"
    echo "  [file]: The filename to open, default to 'recipe.toml'"
    echo ""
    echo "example:"
    echo "  $0 kernel"
    echo "  EDITOR=nano $0 kernel"
    echo "  EDITOR=nvim $0 kernel source"
    exit 1
fi

editor=${EDITOR:-xdg-open}
file=${2:-recipe.toml}

recipe_path=$(make "find.$1" | tr -d '\r')
exec "$editor" "$recipe_path/$file"
