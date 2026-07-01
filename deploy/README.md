# deploy/

This repository is a **logic-only library subset** (`engine`, `mental-poker`,
`mp-wasm`). It contains no server, database, or client, and therefore has no
deployment of its own.

The production service that consumes these crates — HTTP/WebSocket server,
PostgreSQL, web and mobile clients, and its self-hosted infrastructure — is a
separate, closed-source product and is intentionally not part of this repo.

This placeholder exists so the directory layout matches the upstream project.
If you build your own service on top of these crates, put your deployment
manifests here.
