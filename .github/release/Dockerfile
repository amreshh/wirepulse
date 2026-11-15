# The release of an image that's already built (ci.yaml, on main: the image
# its pull request built and smoke-tested), with the release's version and
# commit set for /api/info. Only settings change: no new layers, nothing is
# rebuilt.
#
#   docker build --build-arg IMAGE=ghcr.io/owner/wirepulse@sha256:… \
#     --build-arg RELEASE_VERSION=1.2.3 --build-arg RELEASE_COMMIT=… .github/release
#
# The build args aren't called WIREPULSE_*: the image already has ENV
# variables with those names. Docker (BuildKit) lets an ARG override an
# inherited ENV of the same name, but podman/buildah keeps the ENV, so there
# ${WIREPULSE_COMMIT} would still be the pull request's commit. Other names
# work with both.

ARG IMAGE
FROM ${IMAGE}
ARG RELEASE_COMMIT
ARG RELEASE_VERSION
ENV WIREPULSE_COMMIT=${RELEASE_COMMIT} \
    WIREPULSE_VERSION=${RELEASE_VERSION}
