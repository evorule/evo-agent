# Deploying evo-agent and MCP servers in containers

`evo-agent` does not bundle or require a container runtime. For hardened
deployments where stronger isolation than the built-in MCP lifecycle controls
(Job Object orphan-proofing, per-server memory/CPU caps on Windows) is
required, the recommended shape is to run the agent and its MCP servers
inside containers you control.

## Boundary statement

- The built-in MCP resource limits (`max_memory_mb`, `cpu_rate_percent` in
  `.evo/mcp.toml`) are Windows Job Object based and apply to processes on the
  host. They are a convenience control, not a security boundary.
- Container isolation (image pinning, filesystem/network namespaces, per-
  container resource ceilings) is a deployment-topology concern: build your
  own images for the MCP servers you approve, run them as containers, and
  point `.evo/mcp.toml` at the containerized endpoints (streamable_http
  transport), or run the whole agent inside the container.

## Guidance

1. Build MCP server images yourself (or pin upstream images by digest).
2. Run each server as its own container with explicit CPU/memory ceilings.
3. Keep the approval list (`org.evorule.mcp.servers`) pointing at those
   self-built/pinned definitions - the approval is only as trustworthy as
   the artifact it names.
4. If you run evo-agent itself inside a container, mount only the
   directories the agent needs and pass API keys through the container's
   secret mechanism rather than the host environment.
