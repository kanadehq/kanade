# Developer Quickstart

This guide gets you up and running with a local `kanade` development environment.

## 1. Prerequisites

Before starting, ensure you have the following installed on your Windows machine:
- [Rust toolchain](https://rustup.rs/) (stable channel)
- [cargo-make](https://sagiegurari.github.io/cargo-make/) (run `cargo install --force cargo-make`)
- [bun](https://bun.sh/) (for SPA dependency management and build execution)
- [gsudo](https://github.com/gerardog/gsudo) (for local service deployment tests)
- [nats-server](https://github.com/nats-io/nats-server) (runnable from PATH)

## 2. One-Time Setup

Run the following command at the workspace root to register the git pre-push hooks and install agent skills defined in `apm.yml`:

```powershell
cargo make setup
```

## 3. Launching the Dev Sandbox

You can spin up a fully isolated, multi-component development stack on your local host using a single command:

```powershell
cargo make dev
```

This task runs the following services concurrently in a loopback sandbox:
1. **nats-dev**: Unauthenticated NATS broker listening on port `4223`.
2. **backend-dev**: Dev API server listening on port `8081` with auth disabled.
3. **agent-dev**: Local dev agent talking to the dev NATS broker on `4223`.
4. **web-dev**: Vite dev server for the React SPA listening on `http://localhost:5173`.

Press `Ctrl+C` to tear down all components cleanly.

## 4. Multi-Agent Fleet Simulation

To debug behavior that only shows up when managing multiple machines (e.g. concurrent execution result projection or ID collisions), you can launch a multi-agent sandbox:

```powershell
cargo make dev-fleet
```

This spawns the NATS broker, backend, and SPA, plus three separate dev agents with independent IDs (`dev-pc-1`, `dev-pc-2`, `dev-pc-3`) and isolated state databases.

## 5. Local Deploy Testing

If you want to test the full lifecycle of installing components as Windows services (mirroring production environments), use the local deployment scripts:

```powershell
# Installs CLI, agent, backend, and NATS services locally via gsudo elevation
cargo make local-deploy
```

After deployment, you can verify and interact with the real Windows services. Use the following task to stop and cleanly delete the services when finished:

```powershell
cargo make local-undeploy
```
