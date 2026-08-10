---
name: Service groups include required runtime edges
description: Include proxies, routers, and other edge processes needed to make a group's advertised local URLs work.
date: 2026-08-10
---

# Service groups include required runtime edges

A named development service group must include every runtime process required
for the URLs it advertises. This includes TLS terminators, reverse proxies,
routers, and gateways even when their upstream application services are already
members of the group.

The group's browser-open actions must also use those public edges. When a TLS
route publishes a service's named port, opening that service must use the
configured HTTPS hostname instead of bypassing the edge through localhost.

Verify the group with `aster services up <group> --dry-run`, then exercise its
public URL through a real network client in the canonical end-to-end test.

## Positive example

An `intern` group that advertises `https://intern.dev` includes the frontend,
gateway, and `intern-edge` TLS proxy. Its end-to-end test starts the group and
connects to the HTTPS hostname. The frontend's `[open]` action resolves to
`https://intern.dev`, not its internal localhost listener.

## Counterexample

Starting only the frontend and gateway while documenting an HTTPS URL is
incomplete. A separately running proxy on the developer's machine does not make
the service group self-contained.
