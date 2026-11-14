# The container image (PLAN.md §M14.17 E1): the static (musl) binaries of
# a release on distroless's static base — a /tmp, CA certificates, time
# zones, a user that is not root, nothing else.  The build context is
# scripts/image-context.sh's: the binaries per platform, linux/amd64/ and
# linux/arm64/, and an empty data/.
#
#   scripts/image-context.sh dist ctx
#   docker buildx build --platform linux/amd64,linux/arm64 -f packaging/Dockerfile ctx
#
#   docker run -e NUCLEUS_PASSWORD=… -p 20035:20035 -v nucleus:/data <image>
#   docker run --rm -it <image> sql :memory:
FROM gcr.io/distroless/static-debian12:nonroot
ARG TARGETPLATFORM
# nucleus alone: ncsql is `nucleus sql`.
COPY --chmod=755 ${TARGETPLATFORM}/nucleus /usr/local/bin/
# The database's directory, the image's user's (a volume mounted there
# starts with that owner).
COPY --chown=65532:65532 data/ /data/
VOLUME /data
# The nucleus protocol; PostgreSQL's with --pg-compat 0.0.0.0:5432.
EXPOSE 20035 5432
ENTRYPOINT ["/usr/local/bin/nucleus"]
CMD ["server", "/data", "--listen", "0.0.0.0:20035"]
