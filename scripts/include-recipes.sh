#!/bin/sh

# Print "recipe-name = {}    # <matching line>" for every recipe whose
# recipe.toml matches a pattern, ready to paste into a filesystem config for
# quick testing of WIP recipes.

if [ $# -eq 0 ]
then
    echo "Find matching recipes, and format for inclusion in config"
    echo "Usage: $0 \"pattern\""
    echo "Must be run from the root of the build tree"
    echo "e.g. $0 \"TODO.*error\""
    exit 1
fi

pattern="$*"

find recipes -name recipe.toml -exec grep -l -e "$pattern" {} + | sort |
while IFS= read -r recipe_path
do
    recipe_name="$(basename "$(dirname "$recipe_path")")"
    printf '%s = {}    # %s\n' "$recipe_name" "$(grep -e "$pattern" "$recipe_path" | tr '\n' ' ')"
done
