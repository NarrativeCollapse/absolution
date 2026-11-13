#!/bin/sh

# Show the contents of the "stage" and "sysroot" directories of recipes

if [ $# -eq 0 ]
then
    echo "Show the contents of the 'stage' and 'sysroot' directories in recipe(s)"
    echo "Usage: $0 recipe1 ..."
    echo "Must be run from the root of the build tree"
    echo "e.g. $0 kernel"
    exit 1
fi

find_recipe="target/release/repo"
if [ ! -x "$find_recipe" ]
then
    echo "$find_recipe not found."
    echo "Please run 'make fstools' and try again."
    exit 1
fi

for recipe in "$@"
do
    recipe_dir="$("$find_recipe" find "$recipe")"
    for dir in "$recipe_dir"/target/*/stage "$recipe_dir"/target/*/sysroot
    do
        [ -d "$dir" ] && tree "$dir"
    done
done
