FROM golang:1.26-alpine AS build

WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o /acestream-audio .

FROM docker.io/mwader/static-ffmpeg:8.1.2-amd64
LABEL org.opencontainers.image.source="https://github.com/trexx/docker-acestream-audio"

# The base image ships /ffmpeg and /ffprobe at the filesystem root; the service
# exec's them by name, so "/" must be on PATH.
ENV PATH="/usr/local/bin:/"

COPY --from=build /acestream-audio /usr/local/bin/acestream-audio

USER 65534:65534
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/acestream-audio"]