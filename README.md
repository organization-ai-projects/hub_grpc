# grpc-hub

All-in-one gRPC hub for adapters, events, config, and metrics.

## Features
- gRPC server (tonic/prost) with reflection and health check
- Event streams (Subscribe)
- Auto-load config (`hub.toml`)
- Adapter spawning (process stdio JSON) + built-in echo adapter
- Simple token-based security (optional)
- Limits (message size, timeouts) and backpressure
- Prometheus metrics on `:9090/metrics`
- Graceful shutdown

## Structure
```
grpc-hub/
├─ proto/
│  └─ hub.proto
├─ src/
│  └─ main.rs
├─ hub.toml                 # config (optional)
├─ Cargo.toml
└─ build.rs
```

## Quickstart

### Prerequisites
- Rust stable
- `protoc` (macOS: `brew install protobuf`, Ubuntu: `sudo apt install protobuf-compiler`, Windows: [Download pre-built binaries](https://github.com/protocolbuffers/protobuf/releases), extract, and add to PATH)

### Build & Run
```bash
cargo build
cargo run
```
- gRPC listens on `0.0.0.0:50051`
- Prometheus metrics on `0.0.0.0:9090/metrics`

### Example gRPC calls (grpcurl)
> If `auth_token` is set (e.g. `devtoken123`), add `-H "authorization: Bearer devtoken123"`

- **List services (reflection):**
```bash
grpcurl -plaintext localhost:50051 list
```
- **Open adapter:**
```bash
grpcurl -plaintext -d '{"project_id":"alpha","command":"builtin:echo"}' \
  localhost:50051 hub.v1.Hub/OpenAdapter
```
- **RunOnProject:**
```bash
grpcurl -plaintext -d '{"project_id":"alpha","max_passes":"5","mode":1}' \
  localhost:50051 hub.v1.Hub/RunOnProject
```
- **Subscribe (stream):**
```bash
grpcurl -plaintext -d '{"project_id":"alpha","topics":["diagnostics"]}' \
  localhost:50051 hub.v1.Hub/Subscribe
```
- **Metrics:**
```bash
curl -s localhost:9090/metrics | head
```

## Production notes
- Backpressure: bounded mpsc/broadcast + timeouts
- HTTP/2 keepalive: configured
- Health/Reflection: enabled
- Auth token: header `authorization: Bearer <token>`
- Extensible: add RPCs without breaking contract

---
Ready to use, all-in-one, zero stub, automated end-to-end.
