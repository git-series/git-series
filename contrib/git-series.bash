#!/bin/bash

# Bash completions for git-series
# Copyright © 2016 Dylan Baker

# This program is free software; you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation; either version 2 of the License, or
# (at your option) any later version.

# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
# GNU General Public License for more details.

# You should have received a copy of the GNU General Public License
# along with this program; if not, see <http://www.gnu.org/licenses/>.


__git-series_add() {
    __gitcomp "series base cover -h --help"
}

__git-series_checkout() {
    if [[ "$cur" == -* ]]; then
        __gitcomp "-h --help"
    else
        __gitcomp_nl "$(__git_series)"
    fi
}

__git_series() {
    local dir="$(__gitdir)"
    if [ -d "$dir" ]; then
        git --git-dir "$dir" for-each-ref --format='%(refname:strip=3)' \
            refs/heads/git-series
        return
    fi
}

_git_series() {
    local prev=${COMP_WORDS[COMP_CWORD-1]}
    local cur=${COMP_WORDS[COMP_CWORD]}

    case "${COMP_WORDS[2]}" in
        "add")
            __add
            ;;
        "base")
            if [[ "$cur" == -* ]]; then
                __gitcomp "-h --help -d --delete"
            else
                __git_complete_revlist_file
            fi
            ;;
        "checkout")
            __git-series_checkout
            ;;
        "commit")
            [[ "${prev}" == "-m" ]] && return 1
            __gitcomp "-a --all -v --verbose -m -h --help"
            ;;
        "cover")
            __gitcomp "-d --delete -h --help"
            ;;
        "delete")
            __git-series_checkout
            ;;
        "detach")
            __gitcomp "-h --help"
            ;;
        "format")
            [[ "${prev}" == "--in-reply-to" ]] || \
            [[ "${prev}" == "--reroll-count" ]] || \
            [[ "${prev}" == "-v" ]] && return 1
            __gitcomp "--in-reply-to --no-from --v --reroll-count --stdout -h --help"
            ;;
        "log")
            __gitcomp "-p --patch -h --help"
            ;;
        "rebase")
            if [[ "$cur" == -* ]]; then
                __gitcomp "-i --interactive -h --help"
            else
                __git_complete_revlist_file
            fi
            ;;
        "req")
            if [[ "$cur" == -* ]]; then
                __gitcomp "-p --patch -h --help"
            fi
            # The upstream bash completions don't implement request-pull
            ;;
        "start")
            __gitcomp "-h --help"
            ;;
        "status")
            __gitcomp "-h --help"
            ;;
        "unadd")
            __add
            ;;
        *)  # covers the case of "series" and "help"
            local commands="add base checkout commit cover delete detach format
                            help log rebase req start status unadd"
            __gitcomp "$commands"
            ;;
    esac

    if [[ "$cur" == -* ]]; then
        __gitcomp "-h --help -V --version"
    fi
}

complete -F _git_series git-series
