#!/bin/sh

# Show the package size of recipes, smallest first, in kilobytes.
# Run this inside podman (`make env`).

if [ $# -eq 0 ]
then
    set -- --all-compiled
fi

target="$(target/release/cookbook_redoxer --target)"
# The CSV status column of a built recipe is "Built(<size>)".
target/release/repo cook-list "$@" --display=csv |
    sed -n 's/^\([^,]*\),\([^,]*\),Built(\([0-9]*\)),.*$/\3,\2/p' |
    sort -t , -k 1,1n |
    cut -d , -f 2 |
    while IFS= read -r recipe
    do
        du -k "$recipe/target/$target/stage.pkgar"
    done
