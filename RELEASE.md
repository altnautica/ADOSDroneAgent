# Cutting a release

The agent ships through two channels:

- **Edge (the default).** Every push to `main` that touches the Rust tree runs
  the Rust workflow, which builds the static `aarch64` service binaries
  (`ados-radio`, `ados-groundlink`, `ados-supervisor`, the installer, and so
  on), publishes them to rolling `prebuilt-*` prerelease tags, and mirrors the
  same bytes into a per-revision `rev-<sha>` release. An edge install clones
  `main` (or `--ref <sha>`) and fetches those binaries.

- **Stable.** Pushing a `v<version>` tag runs the release workflow, which
  publishes one release holding everything a stable install places: the Python
  wheel, the deploy bundle (`scripts/`, `data/`, the vendored radio source), the
  installer, and the service binaries built from the tagged Rust tree. A stable
  install (`--channel stable --version <version>`) takes every artifact from
  that release and never from the rolling tags.

Every artifact on both channels is signed with the release minisign key. Its
public half is embedded in the installer and vendored in `scripts/install.sh`;
the private half is the `ADOS_DRIVER_SIGNING_KEY` repository secret. The
bootstrap refuses to run an installer whose signature is missing or does not
verify, on every channel, and the stable channel refuses any unsigned artifact.

## Steps

1. Bump the version in `src/ados/__init__.py` (the single source of truth;
   `pyproject.toml` reads it back through the package metadata).
2. Add the matching `## [<version>]` section to `CHANGELOG.md`.
3. Commit and push to `main`, and wait for the Rust workflow to finish if the
   release changes anything under `crates/`, `data/systemd/`, or the generated
   contract files. The release takes its service binaries from the nearest
   commit that published a `rev-<sha>` release, and refuses to publish if the
   Rust tree changed after that commit.
4. Tag and push the tag to publish the stable release:

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
