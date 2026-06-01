# Cutting a release

The agent ships in two pieces from one push to `main`:

- **Rolling native binaries.** Every push to `main` runs the Rust workflow,
  which builds the static `aarch64` service binaries (`ados-radio`,
  `ados-groundlink`, `ados-supervisor`, the installer, and so on) and publishes
  them to rolling `prebuilt-*` prerelease tags. `scripts/install.sh` fetches the
  prebuilt installer and the installer fetches the rest, so a fresh
  `install.sh --upgrade` picks up the latest binaries without any tag.

- **Versioned wheel + deploy bundle.** Pushing a `v<version>` tag runs the
  release workflow, which builds and signs the Python wheel and the deploy
  bundle and publishes them to a GitHub Release named for that version.

## Steps

1. Bump the version in `src/ados/__init__.py` (the single source of truth;
   `pyproject.toml` reads it back through the package metadata).
2. Add the matching `## [<version>]` section to `CHANGELOG.md`.
3. Commit and push to `main`. The native binaries rebuild and republish
   automatically.
4. Tag and push the tag to publish the versioned wheel + bundle:

   ```bash
   V=$(python -c "import ados; print(ados.__version__)")
   git tag "v$V"
   git push origin "v$V"
   ```

## Why tagging is manual

The tag is created by hand on purpose. A tag pushed by a workflow using the
default Actions token does not re-trigger the release workflow (the platform
blocks workflow-to-workflow triggers from the default token), so an automatic
tag would publish nothing. Tagging from a developer's checkout (or any client
with a real token) triggers the release workflow normally.

## Verifying an upgrade on a device

After a release, an `install.sh --upgrade` on a device should land the new
version. Confirm with `ados version` and, for a real deploy check, compare the
on-disk binary checksum against the published asset:

```bash
sha256sum /opt/ados/bin/ados-radio
```
