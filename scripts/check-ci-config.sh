#!/bin/sh

# List the supported (non-wip) recipes that the CI config does not build

set -e

ARCH="${1:-x86_64}"
config="config/${ARCH}/ci.toml"
repo=target/release/repo
installer=build/fstools/bin/redox_installer

make fstools

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# name<TAB>path of every supported recipe, sorted by name
"$repo" repo-list --all --display=path | grep -v '^recipes/wip/' |
while IFS= read -r recipe_dir
do
    printf '%s\t%s\n' "$(basename "$recipe_dir")" "$recipe_dir"
done | sort -t "$(printf '\t')" -k 1,1 > "$tmp/recipes"

"$installer" --list-packages -c "$config" | sort -u > "$tmp/configured"

echo "Checking for missing packages in ${config}"
printf '%-32s%s\n' "PACKAGE" "RECIPE"
# Recipes whose name is not in the config
join -t "$(printf '\t')" -v 1 "$tmp/recipes" "$tmp/configured" |
while IFS="$(printf '\t')" read -r name recipe_dir
do
    printf '%-32s%s\n' "$name" "$recipe_dir"
done
