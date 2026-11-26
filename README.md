# coderabbit-steward (`cr-steward`)

Budget tracker, scheduler, coverage map, and findings steward for CodeRabbit OSS across `kleinbem` and `kleinbem-dev`.

## Quick start

```bash
# Run the daemon in observe mode (Phase 1 read-only)
cr-steward daemon --listen 127.0.0.1:8484 --database-url sqlite://steward.db?mode=rwc

# Query budget status from the CLI
cr-steward free kleinbem/nix-config
```

## Development

```bash
just test
```
