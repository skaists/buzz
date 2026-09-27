#!/usr/bin/env bash
# Run a built MinIO server + mc image pair the way docker-compose.yml does:
# same server command and credentials, the busybox-wget healthcheck, and the
# same minio-init script. Then round-trip an object. Exits non-zero on any
# failure. Usage: smoke.sh <minio-image-ref> <mc-image-ref>
set -euo pipefail
minio_image="$1"
mc_image="$2"
net="ci-images-smoke-$$"
cleanup() { docker rm -f "$net-minio" >/dev/null 2>&1 || true; docker network rm "$net" >/dev/null 2>&1 || true; }
trap cleanup EXIT

docker network create "$net" >/dev/null
docker run -d --name "$net-minio" --network "$net" --network-alias minio \
  -e MINIO_ROOT_USER=buzz_dev -e MINIO_ROOT_PASSWORD=buzz_dev_secret \
  "$minio_image" server /data --console-address ":9001" >/dev/null

# The compose healthcheck, run inside the container.
for _ in $(seq 1 60); do
  if docker exec "$net-minio" wget -q --spider http://localhost:9000/minio/health/live; then
    healthy=1; break
  fi
  sleep 1
done
if [ -z "${healthy:-}" ]; then
  docker logs "$net-minio" >&2
  echo "minio never became healthy" >&2
  exit 1
fi

# docker-compose.yml's minio-init entrypoint, verbatim, plus a round trip.
docker run --rm --network "$net" --entrypoint /bin/sh "$mc_image" -c "
  mc alias set local http://minio:9000 buzz_dev buzz_dev_secret &&
  mc mb --ignore-existing local/buzz-media &&
  mc anonymous set none local/buzz-media &&
  echo ci-images-smoke > /tmp/obj &&
  mc cp /tmp/obj local/buzz-media/obj &&
  test \"\$(mc cat local/buzz-media/obj)\" = ci-images-smoke
"
echo "smoke ok: $minio_image + $mc_image"
