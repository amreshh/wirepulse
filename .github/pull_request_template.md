<!--
Merging into main releases the next patch version. For a minor or major
release, see "Version on main" at the top of .github/workflows/ci.yaml.
-->

## What and why

<!-- What changes, and the problem it solves. "Fixes #123" closes an issue. -->

## Testing

<!-- What you checked beyond CI (which runs the tests, lint, security,
dependency review, and builds the image and smoke-tests it in demo mode):
commands, what you tried in the app, against which cluster / Cilium version. -->

## Checklist

- [ ] README and comments match the new behaviour
- [ ] Dockerfile changes: the app works in a browser from the image (CI only checks that it starts and answers)
- [ ] deploy/ changes: applied to a cluster, or at least `kubectl apply --dry-run=server -f deploy/`
- [ ] New Kubernetes access: added to the RBAC in `deploy/wirepulse.yaml` and the README's Permissions list
- [ ] Bypasses added to `.github/dependency-review.yaml` say why, and when to look at them again
