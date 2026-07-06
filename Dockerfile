FROM golang:1.24-alpine AS build

WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o /acestream-audio .

FROM alpine:3.22
LABEL org.opencontainers.image.source="https://github.com/trexx/docker-acestream-audio"

RUN apk add --no-cache ffmpeg

COPY --from=build /acestream-audio /usr/local/bin/acestream-audio

USER nobody
EXPOSE 8080
ENTRYPOINT ["acestream-audio"]
