# docker-acestream-audio
A tiny Go service that strips the video track from an AceStream engine stream and serves the audio over plain HTTP. Built for listening in cars and on phones: the full muxed stream (~3–6 Mbps) becomes an audio-only stream (~100–300 kbps), so the mobile-data leg shrinks by an order of magnitude. Pairs with [docker-acestream-webplayer](https://github.com/trexx/docker-acestream-webplayer)'s "Listen" mode.

## Endpoint

```
GET /audio?id=<40-hex content id>[&host=<engine[:port]>][&fmt=adts|mp3]
```

* `id` — the AceStream content id (40 hex chars, required).
* `host` — the AceStream engine to pull from. Optional if the `ENGINE_HOST` env var is set.
* `fmt` — output format. `adts` (AAC, default — browsers and Chromecast Audio both play it, and it keeps the stream-copy path) or `mp3` (fallback for devices that won't play ADTS).

Also serves `/healthz` for probes.

## Copy-first design

The service probes the source with ffprobe, then:

* **AAC source + `fmt=adts`, or MP3 source + `fmt=mp3`** → the audio is **stream-copied untouched** (`-c:a copy`). No re-encode, no quality loss, near-zero CPU.
* **Anything else** (AC3/E-AC3/DTS/MP2 sources, or a format conversion) → transcoded to stereo 128 kbps.

ffmpeg runs per-request and is killed the moment the client disconnects. Note the service still pulls the *full* muxed stream from the engine — the P2P engine cannot serve audio-only — so the savings are on the service→client leg. Run it next to the engine.

Each request connects to the engine with a unique player id (`&pid=audio-…`), so listening through this service doesn't knock out another client (e.g. a browser) playing the same stream directly from the engine.

## Configuration

| Env var | Default | Purpose |
| --- | --- | --- |
| `LISTEN_ADDR` | `:8080` | HTTP listen address |
| `ENGINE_HOST` | *(unset)* | Default AceStream engine `host[:port]` when the request has no `?host=` |

## Deployment

Kubernetes manifests live in [`deploy/`](deploy/): a hardened single-replica Deployment with `/healthz` probes, a ClusterIP Service and an Ingress for `acestream-audio.apps.pixelman.me`. Set the ingress class/TLS bits for your controller (comments inline), then:

```sh
kubectl apply -k deploy/
```

Run it in the same cluster as the engine — it pulls the full muxed stream from the engine per listener.

## Casting to Chromecast Audio

Audio-only cast targets can't decode the muxed TS — hand them this service's stream instead. Chromecast Audio plays AAC, so the default (ADTS) output works and keeps the stream-copy path: the device receives the original audio untouched. The webplayer's cast webhook payload includes a `transcoder` field when an audio host is configured; the Home Assistant automation should call `media_player.play_media` with `http://<transcoder>/audio?id=<id>&host=<engine>` and `media_content_type: "music"` for audio devices. If a device won't play ADTS, append `&fmt=mp3` as a fallback.

## Local development

```sh
podman build -t acestream-audio .
podman run --rm -p 8080:8080 -e ENGINE_HOST=my-engine:6878 acestream-audio
curl -v 'http://127.0.0.1:8080/audio?id=<content id>' | mpv -
```
