#!/usr/bin/env bash
# Read private fixture patterns on stdin; emit counts only, never matched text.
set -euo pipefail
(($#)) || exit 2
awk '
    FILENAME=="/dev/stdin" { if (length($0)) secret[++n]=$0; next }
    {
        bad=($0 ~ /\*[0-9A-Fa-f]{40}([^0-9A-Fa-f]|$)/)
        for (i=1; i<=n; i++) if (index($0,secret[i])) bad=1
        if (bad) matches++
    }
    END {
        printf "AUTH_AUDIT files=%d matching_lines=%d\n",ARGC-2,matches
        exit (matches>0)
    }
' /dev/stdin "$@"
