# deploy/

This repository is the open-source subset (`engine`, `mental-poker`, `mp-wasm`,
`gto-solver`, plus `server-integration/gto_solve.rs` — the published source of
the deployed solver endpoint, offered as AGPL §13 Corresponding Source, not a
runnable server). It contains no database or client and has no deployment of its
own.

The production service that consumes these crates — HTTP/WebSocket server,
PostgreSQL, web and mobile clients, and its self-hosted infrastructure — is a
separate, closed-source product and is intentionally not part of this repo.

This placeholder exists so the directory layout matches the upstream project.
If you build your own service on top of these crates, put your deployment
manifests here.
