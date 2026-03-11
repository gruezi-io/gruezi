# Ansible HA Lab Deploy

This directory contains the Ansible deployment path for a 2-node `gruezi` HA lab.

Today this playbook pushes the local release binary directly. The repository also
contains package-oriented assets under `../contrib/` so future playbooks can
install a built `.deb` or `.rpm` instead of copying the binary by hand.

## Files

- `ansible.cfg`: sets the default inventory and local Ansible defaults for this directory
- `deploy-ha-lab.yml`: deploys the local release binary, renders the HA config, clears stale VIP state, and starts `gruezi`
- `ha-netem.yml`: applies or clears `tc netem` impairment on the HA interface for failure-injection testing
- `ha-drop.yml`: applies or clears an HA-only UDP drop rule to the configured peer on port `9375`
- `ha-collect.yml`: collects `/status`, interface state, qdisc state, and recent `gruezi.log` lines into local artifacts
- `inventory/lab.yml.example`: example YAML inventory for the lab hosts
- `group_vars/gruezi_ha_lab.yml.example`: shared HA defaults example for the lab group
- `templates/gruezi-ha.yaml.j2`: per-node config template rendered on the target hosts

## Prerequisites

- `ansible-playbook` available locally
- SSH access to both hosts
- passwordless `sudo` on both hosts

## Inventory

Create your local lab files first:

```bash
cp inventory/lab.yml.example inventory/lab.yml
cp group_vars/gruezi_ha_lab.yml.example group_vars/gruezi_ha_lab.yml
```

Then update `inventory/lab.yml` with your hostnames, SSH user, peer IPs, priorities, and VIP.

Current host-specific variables:

- `ansible_host`
- `ansible_user`
- `gruezi_node_id`
- `gruezi_peer_ip`
- `gruezi_priority`

Current group-level lab variable in inventory:

- `gruezi_vip`

Shared defaults live in `group_vars/gruezi_ha_lab.yml`, including:

- `gruezi_interface`
- `gruezi_group_id`
- `gruezi_shared_key`
- `gruezi_advert_interval_ms`

## Run

From the `ansible/` directory:

```bash
ansible-playbook deploy-ha-lab.yml
```

From the repository root:

```bash
ansible-playbook -i ansible/inventory/lab.yml ansible/deploy-ha-lab.yml
```

## What It Does

The playbook:

1. runs `cargo build --release --locked` locally
2. compares the local release binary with the remote one
3. copies the release binary to each node only when it changed
4. renders `/home/<ansible_user>/gruezi-lab/gruezi.yaml`
5. restarts `gruezi` only when the binary changed, the config changed, or the process is missing
6. removes any stale VIP from the configured interface before restart
7. queries `http://127.0.0.1:9376/status` on each host

## HA Impairment Testing

The Ansible lab can also inject `tc netem` impairments on the HA interface of the live lab nodes.

This is useful when you want to reproduce the same kinds of scenarios documented in the root README against the real lab hosts instead of local namespaces.

The playbook targets HA traffic only:

- it matches outbound UDP traffic to the configured peer IP on port `9375`
- it does not intentionally impair the full interface
- this avoids cutting off the SSH control path during lab runs

For safe one-way-loss testing on a live shared-management interface, prefer the HA-only drop playbook instead:

```bash
ansible-playbook ha-drop.yml --limit gruezi-a
```

Apply bounded delay and jitter to both lab nodes:

```bash
ansible-playbook ha-netem.yml -e "gruezi_netem_args=delay 250ms 50ms"
```

Create a full partition by dropping all HA traffic on both nodes:

```bash
ansible-playbook ha-netem.yml -e "gruezi_netem_args=loss 100%"
```

Create one-way loss by limiting the playbook to one node:

```bash
ansible-playbook ha-netem.yml --limit gruezi-a -e "gruezi_netem_args=loss 100%"
```

Clear the impairment:

```bash
ansible-playbook ha-netem.yml -e "gruezi_netem_state=absent"
```

Collect artifacts before or after a scenario:

```bash
ansible-playbook ha-collect.yml
```

Use a custom artifact label when you want multiple snapshots for the same exercise:

```bash
ansible-playbook ha-collect.yml -e "gruezi_artifact_label=before-one-way-loss"
ansible-playbook ha-drop.yml --limit gruezi-a
ansible-playbook ha-collect.yml -e "gruezi_artifact_label=after-one-way-loss"
ansible-playbook ha-drop.yml --limit gruezi-a -e "gruezi_drop_state=absent"
ansible-playbook ha-collect.yml -e "gruezi_artifact_label=after-heal"
```

Operational notes:

- the playbook uses `gruezi_interface` from the HA lab inventory or group vars
- the playbook also uses each host's `gruezi_peer_ip` to match only HA traffic to the configured peer
- `--limit` is the easiest way to create asymmetric impairment on only one host
- `ha-drop.yml` is the safer choice for one-way packet loss on a live lab that shares the same NIC for SSH and HA traffic
- after applying or clearing an impairment, validate the result with `gruezi status --watch` or by checking `http://127.0.0.1:9376/status` on each node
- a full partition in a 2-node HA pair can produce dual-active ownership until connectivity is restored
- collected artifacts are written locally under `ansible/artifacts/<label>/`
- each collection stores one file per host for `/status`, `tc qdisc show`, `ip address show`, and the recent `gruezi.log` tail

Observed live-lab result:

- in the local ignored lab config, `gruezi_preempt` was set to `true`
- during one-way HA packet loss from `gruezi-a` to `gruezi-b`, the lower-priority peer promoted on timeout
- once `gruezi-a` saw `gruezi-b` newly become `Active`, it demoted with `decision_reason=peer_became_active_conflict`
- after the impairment was removed, the pair stayed converged as `gruezi-b` `Active` and `gruezi-a` `Standby`

If you want the safer application-VIP behavior described in the root README, set:

```yaml
gruezi_preempt: false
```

## Notes

- The remote install directory defaults to `/home/<ansible_user>/gruezi-lab`.
- The playbook always builds the local release binary and uses that artifact for deployment. It does not build on the remote hosts.
- The VIP is intended to be environment-specific, so it is defined in the inventory.
- `inventory/lab.yml` and `group_vars/gruezi_ha_lab.yml` are intentionally ignored so local lab IPs and shared keys do not get committed by accident.
- `ansible.cfg` is picked up automatically when you run `ansible-playbook` from this directory.
