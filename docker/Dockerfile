# Minimal image carrying only the veneer binary, for use in other Dockerfiles:
#   COPY --from=ghcr.io/lukasparke/veneer:1 /veneer /usr/local/bin/veneer
# Build: docker build -f docker/Dockerfile --build-arg BIN=target/linux/release/veneer -t veneer .
FROM scratch
ARG BIN=target/linux/release/veneer
COPY ${BIN} /veneer
ENTRYPOINT ["/veneer"]
