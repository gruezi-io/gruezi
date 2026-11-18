# Compatibility

The planned compatibility target is the narrow HTTP surface used by Patroni's
etcd3 integration. It is not a claim of general etcd compatibility.

**Status: Planned.**

The intended endpoints are:

* `POST /v3/kv/range`
* `POST /v3/kv/put`
* `POST /v3/kv/deleterange`
* `POST /v3/kv/txn`
* `POST /v3/lease/grant`
* `POST /v3/lease/keepalive`
* `POST /v3/watch`
* `POST /v3/auth/authenticate`
* `POST /v3/cluster/member/list`
* `GET /version`

The request and response shapes, lease timing, watch behavior, authentication,
and error mapping must be verified against a pinned Patroni fixture before this
surface is called compatible. No claim is made for other etcd clients,
unlisted endpoints, gRPC behavior, or drop-in replacement use.
