#!/usr/bin/env bash
# Sourced only by run-vm.sh. Input is data, never shell or Ansible code.
deny() {
    printf 'REFUSED: %s\n' "$*" >&2
    local denied_row
    for denied_row in "${rows[@]}"; do
        printf '%s\tNOT RUN\treason=%s\n' "$denied_row" "$*"
    done
    exit 2
}
reference() {
    local value="${1,,}"
    [[ "$value" == *maria-a* || "$value" == *maria-b* ||
        "$value" == *192.168.255.50* || "$value" == *192.168.255.51* ||
        "$value" == *192.168.255.52* ]]
}
private_ipv4() {
    local address="$1" a b c d
    [[ "$address" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || return 1
    IFS=. read -r a b c d <<<"$address"
    local octet
    for octet in "$a" "$b" "$c" "$d"; do
        [[ "$octet" == 0 || "$octet" != 0* ]] || return 1
        ((10#$octet <= 255)) || return 1
    done
    ((a == 10 || (a == 172 && b >= 16 && b <= 31) || (a == 192 && b == 168))) &&
        ((d > 0 && d < 255)) && ! reference "$address"
}
read_inventory() {
    local file="$1" line name host user interface vip extra
    [[ -f "$file" && ! -L "$file" ]] || deny 'inventory must be a regular static file'
    reference "$file" && deny 'reference inventory path'
    local content
    content="$(<"$file")"
    reference "$content" && deny 'reference name or address in inventory'
    names=() addresses=() users=() interfaces=() vips=()
    local header=0
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -z "$line" || "$line" == \#* ]] && continue
        if ((header == 0)); then
            [[ "$line" == '[gruezi_ha_lab]' ]] || deny 'unsupported inventory header'
            header=1
            continue
        fi
        read -r name host user interface vip extra <<<"$line"
        [[ -z "$extra" && "$name" =~ ^[a-z][a-z0-9-]{0,31}$ &&
            "$host" == ansible_host=* && "$user" == ansible_user=* &&
            "$interface" == gruezi_interface=* && "$vip" == gruezi_vip=* ]] ||
            deny 'unsupported or ambiguous inventory row'
        host="${host#*=}" user="${user#*=}" interface="${interface#*=}" vip="${vip#*=}"
        private_ipv4 "$host" || deny 'target must be canonical private IPv4, never DNS or an alias'
        [[ "$user" =~ ^[a-z_][a-z0-9_-]{0,31}$ &&
            "$interface" =~ ^[a-zA-Z][a-zA-Z0-9_]{0,14}$ &&
            "$vip" == */32 ]] || deny 'unsupported user, interface or VIP'
        private_ipv4 "${vip%/32}" || deny 'VIP must be canonical private IPv4 /32'
        names+=("$name") addresses+=("$host") users+=("$user") interfaces+=("$interface") vips+=("$vip")
    done <"$file"
    ((${#names[@]} == 2)) || deny 'exactly two disposable hosts required'
    [[ "${names[0]}" != "${names[1]}" && "${addresses[0]}" != "${addresses[1]}" &&
        "${vips[0]}" == "${vips[1]}" && "${vips[0]%/32}" != "${addresses[0]}" &&
        "${vips[0]%/32}" != "${addresses[1]}" ]] || deny 'duplicate or inconsistent hosts or VIP'
}
read_allowlist() {
    local file="$root/ansible/vm-allowlist.txt" line name address extra
    [[ -f "$file" && ! -L "$file" ]] || deny 'missing explicit ansible/vm-allowlist.txt'
    local -A allowed=()
    while IFS= read -r line || [[ -n "$line" ]]; do
        [[ -z "$line" || "$line" == \#* ]] && continue
        reference "$line" && deny 'reference entry in allowlist'
        read -r name address extra <<<"$line"
        [[ -z "$extra" && "$name" =~ ^[a-z][a-z0-9-]{0,31}$ ]] ||
            deny 'invalid allowlist row'
        private_ipv4 "$address" || deny 'invalid allowlist address'
        [[ ! -v "allowed[$name]" ]] || deny 'duplicate allowlist host'
        allowed["$name"]="$address"
    done <"$file"
    local i
    for i in 0 1; do
        [[ "${allowed[${names[i]}]:-}" == "${addresses[i]}" ]] ||
            deny 'host and address not in allowlist'
    done
}
