#!/bin/sh

# List the executables each recipe installs, to find name conflicts

usage() {
    echo "List executable names to find duplicates"
    echo "Usage: $0 [-h] [-a] [-arm64 | -i586] [recipes]"
    echo "Default architecture is x86_64, -arm64 is aarch64, -i586 is i586"
    echo "Only duplicates are listed unless -a is specified"
    echo "-h is this message"
    exit 0
}

repo=target/release/repo
recipes=""
target="x86_64-unknown-redox"
all=false

for arg in "$@"
do
    case "$arg" in
        -arm64) target="aarch64-unknown-redox";;
        -i586|-i686) target="i586-unknown-redox";;
        -a) all=true;;
        -h) usage;;
        *) recipes="$recipes $arg";;
    esac
done

if [ -z "$recipes" ]
then
    recipes="$("$repo" repo-list --all-compiled --display=path)"
fi

# Print "<recipe path> <executable>" lines; then keep every line whose
# executable name occurs more than once, unless -a was given.
for recipe in $recipes
do
    case "$recipe" in
        */*) recipe_path="$recipe";;
        *) recipe_path="$("$repo" find "$recipe")";;
    esac
    for dir in "$recipe_path/target/$target/stage/usr/bin" "$recipe_path/target/$target/stage/bin"
    do
        [ -d "$dir" ] || continue
        find "$dir" -type f | while IFS= read -r command
        do
            echo "$recipe_path $(basename "$command")"
        done
    done
done | sort -k 2 | if [ "$all" = true ]
then
    cat
else
    awk '{ count[$2]++; line[NR] = $0; name[NR] = $2 }
         END { for (i = 1; i <= NR; i++) if (count[name[i]] > 1) print line[i] }'
fi
