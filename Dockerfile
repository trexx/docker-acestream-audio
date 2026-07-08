FROM golang:1.26-alpine@sha256:0178a641fbb4858c5f1b48e34bdaabe0350a330a1b1149aabd498d0699ff5fb2 AS build

WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o /acestream-audio .

FROM docker.io/mwader/static-ffmpeg:8.1.2-amd64@sha256:3bfa407c614a29a4535f1e3220fd9f6bc9cd7c25483036962e3c8ff711b56e01
LABEL org.opencontainers.image.source="https://github.com/trexx/docker-acestream-audio"

# The base image ships /ffmpeg and /ffprobe at the filesystem root; the service
# exec's them by name, so "/" must be on PATH.
ENV PATH="/usr/local/bin:/"

COPY --from=build /acestream-audio /usr/local/bin/acestream-audio

USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/acestream-audio"]