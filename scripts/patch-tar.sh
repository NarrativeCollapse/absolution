#!/bin/sh

set -e

# Generate a "redox.patch" for a tar-based recipe from the changes made in
# its source directory

if [ -z "$1" ]
then
    echo "Generates a 'redox.patch' for a tar-based recipe"
    echo ""
    echo "Usage: $0 <recipe-name>"
    exit 1
fi

recipe_path=$(make "find.$1" | tr -d '\r')

if [ ! -d "$recipe_path/source" ]; then
    echo "Error: 'source' directory not found"
    exit 1
fi

if [ ! -f "$recipe_path/source.tar" ]; then
    echo "Error: 'source.tar' not found"
    exit 1
fi

export TZ=UTC
set -x

cd "$recipe_path"
mv source source-new
# Unpack the pristine source, dropping the archive's top-level directory.
mkdir source.extract
(cd source.extract && tar xf ../source.tar)
mv source.extract/* source
rmdir source.extract
diff -ruwN source source-new > redox.patch || true
rm -rf source
mv source-new source
