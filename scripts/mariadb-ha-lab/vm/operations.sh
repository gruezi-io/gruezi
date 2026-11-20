#!/usr/bin/env bash
# Fixed operation catalogue. Ansible runs from a private snapshot without
# adjacent inventory/group_vars, user config, proxy aliases, or ambient env.
    scratch="$(mktemp -d "${TMPDIR:-/tmp}/gruezi-vm-run.XXXXXX")"
    chmod 700 "$scratch"
    cleanup_kind='' result=0
    restore() {
        local status=0
        play "$cleanup_kind" absent || status=$?
        if ((status != 0)); then
            printf 'CLEANUP UNCONFIRMED: kind=%s target=%s cleanup_exit=%s; manual recovery required\n' \
                "$cleanup_kind" "$target" "$status" >&2
            return "$status"
        fi
        cleanup_kind=''
    }
    cleanup() {
        local original=$? status=0
        trap - EXIT
        if [[ -n "$cleanup_kind" ]]; then
            restore || status=$?
            ((status == 0)) || original="$status"
        fi
        rm -rf -- "$scratch"
        exit "$original"
    }
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    mkdir -m 700 -- "$output"
    [[ -f "${HOME:?}/.ssh/known_hosts" ]] || deny 'pinned ~/.ssh/known_hosts required'
    cp "$HOME/.ssh/known_hosts" "$scratch/known_hosts"
    i=0 peer=0
    for i in 0 1; do
        peer=$((1-i))
        printf '%s ansible_host=%s ansible_user=%s gruezi_interface=%s gruezi_peer_ip=%s\n' \
            "${names[i]}" "${addresses[i]}" "${users[i]}" "${interfaces[i]}" "${addresses[peer]}" \
            >>"$scratch/hosts"
    done
    # INI header inserted by writing the complete bounded inventory once.
    printf '[gruezi_ha_lab]\n%s\n' "$(<"$scratch/hosts")" >"$scratch/inventory"
    cp "$root"/ansible/ha-{drop,netem,collect}.yml "$scratch/"
    printf '[defaults]\nhost_key_checking=True\nretry_files_enabled=False\n' >"$scratch/ansible.cfg"
    safe_path="$PATH" observation=0
    cd -- "$scratch"
    ssh_args="-F /dev/null -o ProxyCommand=none -o ProxyJump=none -o CanonicalizeHostname=no -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$scratch/known_hosts"
    play() {
        local kind="$1" state="${2:-present}" vars=''
        case "$kind" in
            drop) vars="gruezi_drop_state=$state" ;;
            netem) vars="{\"gruezi_netem_state\":\"$state\",\"gruezi_netem_args\":\"loss 100%\"}" ;;
            collect)
                observation=$((observation+1))
                vars="gruezi_artifact_root=$output gruezi_artifact_label=observation-$observation" ;;
        esac
        env -i PATH="$safe_path" HOME="$scratch" ANSIBLE_CONFIG="$scratch/ansible.cfg" \
            timeout 90 ansible-playbook -i "$scratch/inventory" "$scratch/ha-$kind.yml" \
            --limit "$([[ "$kind" == collect ]] && printf gruezi_ha_lab || printf '%s' "$target")" \
            --ssh-common-args "$ssh_args" -e "$vars"
    }
    selected=0
    [[ "$target" == "${names[0]}" ]] || selected=1
    remote() {
        env -i PATH="$safe_path" HOME="$scratch" timeout 150 ssh -F /dev/null \
            -o ProxyCommand=none -o ProxyJump=none -o CanonicalizeHostname=no \
            -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=yes \
            -o "UserKnownHostsFile=$scratch/known_hosts" \
            "${users[selected]}@${addresses[selected]}" \
            sudo -n env "GRUEZI_ALLOW_WATCHDOG_RESET=$([[ "$row" == watchdog-reset ]] && printf 1 || printf 0)" bash -s -- "$1" \
            | tee "$output/$row.log"
    }
    play collect
    case "$row" in
        firewall-block|asymmetric-loss)
            cleanup_kind=drop
            [[ "$row" != asymmetric-loss ]] || cleanup_kind=netem
            play "$cleanup_kind"
            play collect
            restore ;;
        kernel-sigkill)
            remote "${vips[selected]}" <"$root/scripts/mariadb-ha-lab/vm/kernel-sigkill.sh" ;;
        watchdog-arm-failure)
            remote arm-failure <"$root/scripts/mariadb-ha-lab/vm/watchdog-arm-failure.sh" ;;
        watchdog-reset)
            # The original script tests required-watchdog arm/feed/disarm/reset
            # in an isolated namespace. It resets this explicitly named VM.
            remote reset <"$root/scripts/test-watchdog-softdog.sh" || result=$?
            # A disconnect alone cannot prove a reset.
            printf 'watchdog-reset: reset and reboot identity require post-reboot operator collection\n' ;;
    esac
    play collect || result=$?
    printf '%s\tEXECUTED\ttransport_exit=%s verdict=UNKNOWN independent evidence requires review\n' "$row" "$result"
    printf 'MariaDB read_only is an Experimental write restriction, never fencing; SIGKILL may leave a residual writer.\n'
    exit "$result"
