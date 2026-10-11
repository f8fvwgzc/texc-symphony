# texc-symphony runbook

## Health

- symphony: confirm the process answers its health endpoint and the container is running
- symphony-web: confirm the process answers its health endpoint and the container is running
- xtask: confirm the process answers its health endpoint and the container is running

## Logs

- `docker logs --tail 200 <container>` (single container)
- `docker compose logs -f app` (Compose)

## Rollback

Rollback order is the reverse of the plan; every step is undone from its last checkpoint:

- Undo `pull-request` (repo.open_pull_request)
- Undo `tests` (tests.run_local)
- Undo `externalise` (docs.generate)
- Undo `inventory` (repo.analyze)
- Undo `checkout` (repo.checkout)
- Undo `analyze` (repo.analyze)

Redeploying the previous image tag is always safe: the deploy workflow accepts any tag the registry still holds.

## Escalation

Every failure pauses the execution, blocks dependents and waits for a human to pick a remediation (CONTRACT.md §7).
