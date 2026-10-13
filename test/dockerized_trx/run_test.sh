#!/bin/bash -e
# Builds the images, runs the whole test suite and exits with the test result.

# jack2 runs the Debian build. On x86 hosts it's a 32-bit (i386) build, the only 32-bit coverage
# in this suite; other hosts build for their native platform. Override with e.g. DEBIAN_PLATFORM=linux/386.
if [ -z "$DEBIAN_PLATFORM" ]; then
    docker_arch=$(docker version -f '{{.Server.Arch}}')
    case "$docker_arch" in
        amd64) DEBIAN_PLATFORM=linux/386 ;;
        *) DEBIAN_PLATFORM=linux/$docker_arch ;;
    esac
fi
export DEBIAN_PLATFORM
echo "Debian (jack2) platform: $DEBIAN_PLATFORM"

# PENDING test of two Inferno devices sharing one IP: netaudio 0.4 keys devices by IP ("control address ... is
# ambiguous across devices"), so it skips their status probes and a pass gets too slow for the time budget.
# Enable with INFERNO_TEST_SAME_HOST_DEVICES=1.
if [ "$INFERNO_TEST_SAME_HOST_DEVICES" = 1 ]; then
    export COMPOSE_PROFILES=same_host
    echo "Same-host devices test enabled"
fi
export INFERNO_TEST_SAME_HOST_DEVICES

# Network latency of every device. Must exceed the host's worst-case scheduling latency (see README: Audio
# stability tips): Docker Desktop's VM gets paused by the host for up to ~20 ms, hence the 40 ms default
# (Dante's maximum). On a tuned Linux host, test tight latency with e.g. INFERNO_TEST_LATENCY_NS=10000000.
export INFERNO_TEST_LATENCY_NS=${INFERNO_TEST_LATENCY_NS:-40000000}
echo "Network latency: $INFERNO_TEST_LATENCY_NS ns"

pushd ../.. > /dev/null
docker build -f Dockerfile.alpine-alsa -t inferno_aoip:alpine-alsa --build-arg BUILD_FLAGS= --build-arg BUILD_TYPE=debug .
docker build -f Dockerfile.fedora-alsa -t inferno_aoip:fedora-alsa --build-arg BUILD_FLAGS= --build-arg BUILD_TYPE=debug .
docker buildx build --load --platform "$DEBIAN_PLATFORM" -f Dockerfile.debian-alsa -t inferno_aoip:debian-alsa --build-arg BUILD_FLAGS= --build-arg BUILD_TYPE=debug .
docker build -f Dockerfile.alpine-i2pipe -t inferno_aoip:alpine-i2pipe --build-arg BUILD_FLAGS= --build-arg BUILD_TYPE=debug .
popd > /dev/null

docker compose down -v
# Not `up --exit-code-from`: it implies --abort-on-container-exit, which Compose v2/v5 also triggers when the
# one-shot `init` service exits, leaving the stack half-stopped and `up` hanging after the controller exits.
docker compose up --build --detach
docker compose logs --follow &
logs_pid=$!
set +e
docker compose wait control_and_test > /dev/null
result=$(docker inspect -f '{{.State.ExitCode}}' "$(docker compose ps -aq control_and_test)")
[ -n "$result" ] || result=1
set -e
kill $logs_pid 2>/dev/null || true
echo "control_and_test exited with $result"
if [ -n "$ARTIFACTS_DIR" ]; then
    mkdir -p "$ARTIFACTS_DIR"
    docker compose cp control_and_test:/shared/. "$ARTIFACTS_DIR/" || true
fi
docker compose down -v
exit $result
