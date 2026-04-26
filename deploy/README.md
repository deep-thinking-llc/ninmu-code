# Claw Deployment

Quick-start deployment configs for the Claw CLI agent.

## Local Build

```bash
./deploy/deploy.sh local     # Build release binary
./deploy/deploy.sh docker    # Build Docker image + smoke test
./deploy/deploy.sh docker-rpc # Start RPC server in Docker
```

## Docker Compose

```bash
cd deploy
docker compose up -d
```

## Fly.io

```bash
flyctl launch --dockerfile deploy/Dockerfile
cp deploy/fly.toml.example deploy/fly.toml
./deploy/deploy.sh fly
```

## Files

| File | Purpose |
|---|---|
| `Dockerfile` | Multi-stage build (Rust → Debian slim) |
| `docker-compose.yml` | Agent + optional RPC sidecar |
| `deploy.sh` | One-command deploy for local/Docker/Fly |
