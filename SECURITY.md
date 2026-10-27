# Security policy

## Supported versions

openab supports only the latest minor release line. That line includes its stable releases and the betas toward the next release. Older minor lines get no fixes, and maintainers do not backport fixes to them.

For example, when `0.10.0` is the latest stable release, `0.10.x` and the betas toward the next release are supported. `0.9.x` is not.

To get security fixes, upgrade to the latest stable release.

## Report a vulnerability

Do not open a public issue, pull request, or discussion for a vulnerability that is not yet public. Report it privately through GitHub:

1. Open the [Security tab](https://github.com/openabdev/openab/security) of this repository.
2. Click **Report a vulnerability**.
3. Describe the problem, the affected version or image tag, and the steps to reproduce it.

Maintainers follow up in the private advisory that your report creates.

A vulnerability that is already public, such as a published CVE in a dependency, can go in a regular issue.

## Dependencies and base images

Release images include upstream components: the Node.js base image, the agent CLIs, and Debian packages. When one of them has a CVE, maintainers rebuild the images on the supported line and release a new version. The fix ships as a new beta. Maintainers then promote that beta to stable without a rebuild.

- A fix never overwrites an existing version tag such as `0.10.0-claude`. It always ships under a new version.
- The floating tags `stable-<agent>`, `<major.minor>-<agent>`, and `beta-<agent>` move to the fixed build.
- `nightly-<agent>` floats and has no compatibility guarantee. It is not a release.
- The deprecated per-agent repositories, such as `ghcr.io/openabdev/openab-claude`, receive no updates and will be deleted. Move to the unified tags in [Docker image tags](docs/image-tags.md).

The `node-security-check.yml` workflow runs daily. It opens or updates one issue labeled `node-security` when a Node.js security release is newer than the pinned `NODE22_VERSION` or `NODE24_VERSION`, or newer than the Node.js in the published `stable-*` or `beta-*` images. Maintainers then follow [Base Image Security Release](RELEASING.md#base-image-security-release) in `RELEASING.md`.
