#!/bin/sh

# Show what changed in each component since the last release tag

set -e

LAST_RELEASE_TAG="$(git describe --tags --abbrev=0)"
LAST_RELEASE_TIMESTAMP="$(git log --format="%ct" -1 "${LAST_RELEASE_TAG}")"
echo "Last release: ${LAST_RELEASE_TAG} at ${LAST_RELEASE_TIMESTAMP}"

summary=false
mdlinks=false
if [ "$1" = "--summary" ]
then
    summary=true
elif [ "$1" = "--mdlinks" ]
then
    mdlinks=true
fi

repo=target/release/repo
installer=build/fstools/bin/redox_installer
bold="$(printf '\033[1m')"
normal="$(printf '\033[0m')"

# name=path lines, one per component
list_repos() {
    echo "build-system=."
    # TODO: resolve dependencies instead of manually adding these initfs packages
    for package in $("$installer" --list-packages -c "config/$(uname -m)/ci.toml") \
        init logd ramfs randd zerod
    do
        echo "${package}=$("$repo" find "${package}")/source"
    done
}

list_repos | while IFS= read -r name_repo
do
    name="${name_repo%%=*}"
    path="${name_repo#*=}"
    if [ "${summary}" = true ]
    then
        printf '\n### %s\n\n' "${name}"
    elif [ "${mdlinks}" = true ]
    then
        printf '%s' "- [${name}]"
    else
        printf '%s ' "${bold}${name}:${normal}"
    fi

    if [ -e "${path}/.git" ]
    then
        remote="$(git -C "${path}" remote get-url origin)"
        website="${remote%.git}"
        before="$(git -C "${path}" log --until="${LAST_RELEASE_TIMESTAMP}" --format="%h" -1)"
        after="$(git -C "${path}" log --since="${LAST_RELEASE_TIMESTAMP}" --format="%h" -1)"
        if [ -z "${before}" ]
        then
            echo "New repository at ${website}"
        elif [ -z "${after}" ]
        then
            echo "No changes"
        elif [ "${summary}" = true ]
        then
            git -C "${path}" log "${before}...${after}" --oneline
        elif [ "${mdlinks}" = true ]
        then
            echo "(${website}/-/compare/${before}...${after})"
        else
            echo "${website}/-/compare/${before}...${after}"
        fi
    else
        echo "Not a git repository"
    fi
done
