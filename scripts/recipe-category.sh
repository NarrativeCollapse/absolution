#!/bin/sh

# Run a make recipe action over every recipe in a category

if [ -z "$1" ] || [ -z "$2" ]
then
    echo "Build or clean all recipe directories in a category" >&2
    echo "Usage: $0 <action> <recipe-category>" >&2
    echo "<action> can be f, r, c, u, p, or combinations that \"make\" understands" >&2
    echo "<category> is the path of a category, e.g. \"core\", \"wip\", \"wip/dev\"" >&2
    exit 1
fi

# make cannot pass "/" in a pattern, so the cookbook accepts "." instead.
category=$(printf '%s' "$2" | tr '/' '.')
make "${1#-}.--category-$category"
