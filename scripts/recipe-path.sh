#!/bin/sh

# Return list of paths from recipes

IFS=","
make "find.$*"
