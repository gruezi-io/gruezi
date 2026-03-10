# Ansible HA Lab Deploy

This directory contains the Ansible deployment path for a 2-node `gruezi` HA lab.

Today this playbook pushes the local release binary directly. The repository also
contains package-oriented assets under `../contrib/` so future playbooks can
install a built `.deb` or `.rpm` instead of copying the binary by hand.

## Files

- `ansible.cfg`: sets the default inventory and local Ansible defaults for this directory
- `deploy-ha-lab.yml`: deploys the local release binary, renders the HA config, clears stale VIP state, and starts `gruezi`
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

## Notes

- The remote install directory defaults to `/home/<ansible_user>/gruezi-lab`.
- The playbook always builds the local release binary and uses that artifact for deployment. It does not build on the remote hosts.
- The VIP is intended to be environment-specific, so it is defined in the inventory.
- `inventory/lab.yml` and `group_vars/gruezi_ha_lab.yml` are intentionally ignored so local lab IPs and shared keys do not get committed by accident.
- `ansible.cfg` is picked up automatically when you run `ansible-playbook` from this directory.
