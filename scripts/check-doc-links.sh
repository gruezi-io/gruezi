#!/bin/bash
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
status=0

check_file() {
    file=$1
    while IFS= read -r link; do
        case "$link" in
            ""|\#*) continue ;;
        esac
        case "$link" in
            http://*|https://*|mailto:*|git@*|//*) continue ;;
        esac

        target=${link%%\#*}
        anchor=
        case "$link" in
            *\#*) anchor=${link#*\#} ;;
        esac

        if [ -n "$target" ]; then
            case "$target" in
                /*) path=$root$target ;;
                *) path=$(CDPATH= cd -- "$(dirname "$file")" && pwd)/$target ;;
            esac
            if [ ! -f "$path" ]; then
                printf '%s: broken link %s\n' "$file" "$link" >&2
                status=1
                continue
            fi
        else
            path=$file
        fi

        if [ -n "$anchor" ]; then
            if ! awk -v wanted="$anchor" '
                function slug(value, result) {
                    result = tolower(value)
                    gsub(/<[^>]*>/, "", result)
                    gsub(/[^a-z0-9 _-]/, "", result)
                    gsub(/[ _]+/, "-", result)
                    return result
                }
                /^#/ {
                    heading = $0
                    sub(/^#+[[:space:]]*/, "", heading)
                    if (slug(heading) == wanted) found = 1
                }
                END { exit(found ? 0 : 1) }
            ' "$path"; then
                :
            else
                printf '%s: broken anchor %s\n' "$file" "$link" >&2
                status=1
            fi
        fi
    done <<EOF
$(awk '
    {
        line = $0
        while (match(line, /\[[^]]+\]\([^)]*\)/)) {
            item = substr(line, RSTART, RLENGTH)
            sub(/^[^)]*\(/, "", item)
            sub(/\)$/, "", item)
            print item
            line = substr(line, RSTART + RLENGTH)
        }
    }
' "$file")
EOF
}

check_file "$root/README.md"
for file in "$root"/docs/*.md; do
    check_file "$file"
done

if [ "$status" -eq 0 ]; then
    printf 'documentation links: ok\n'
fi
exit "$status"
