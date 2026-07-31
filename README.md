# docker-acestream-audio
A tiny Rust service that pulls an AceStream engine stream **once** and re-serves it to many listeners — as audio-only (for cars and phones) or as fragmented MP4 with the video track copied untouched (for browsers). The full muxed stream (~3–6 Mbps) becomes an audio stream of ~100–300 kbps, so the mobile-data leg shrinks by an order of magnitude. Pairs with [docker-acestream-webplayer](https://github.com/trexx/docker-acestream-webplayer)'s "Listen" mode.

## Endpoints

```
GET /audio?id=<40-hex content id>[&fmt=adts|mp3]
GET /video?id=<40-hex content id>
GET /status
GET /healthz
```

* `id` — the AceStream content id (40 hex chars, required).
* `fmt` — audio output format. `adts` (AAC, default — browsers and Chromecast Audio both play it, and it keeps the stream-copy path) or `mp3` (fallback for devices that won't play ADTS).

`/video` serves fragmented MP4 suitable for MSE playback in a browser. The engine to pull from comes from the required `ENGINE_HOST` env var.

## One engine pull per content id

Every listener used to cost a separate full-bitrate pull from the engine. Now a single puller holds one engine session per content id and fans the raw MPEG-TS out to one ffmpeg per output format, each of which fans out to its own listeners:

```
engine ──1 pull──> puller ──raw TS──┬──> ffmpeg (adts) ──> listeners
                                    └──> ffmpeg (fmp4) ──> listeners
```

So a second listener costs the engine nothing, whether or not they want the same format. Teardown is refcounted from the bottom up: the last listener leaving a format stops its ffmpeg, and the last format stopping closes the engine session (with an explicit `method=stop`, rather than waiting for the connection to lapse).

The source is probed **once** per content id, on a buffer of the shared pull, so listeners after the first start with no probe delay at all.

## Copy-first design

| Endpoint | Source audio | Video | Audio |
| --- | --- | --- | --- |
| `/audio&fmt=adts` | AAC | — | **copied** |
| `/audio&fmt=adts` | anything else | — | transcoded to AAC 128k |
| `/audio&fmt=mp3` | MP3 | — | **copied** |
| `/audio&fmt=mp3` | anything else | — | transcoded to MP3 128k |
| `/video` | AAC | **copied** | **copied** |
| `/video` | anything else | **copied** | transcoded to AAC 128k |

Video is always stream-copied — that is where the CPU saving is, and no pixel is ever decoded. Audio is copied when the source codec is already browser-playable. The asymmetry on `/video` is not optional: AC-3, E-AC-3 and MP2 are legal inside MP4 but browsers will not decode them, so a blanket `-c copy` would produce video with silence.

## Latency

Tuned for low latency throughout: `-flush_packets 1` and `-avioflags direct` stop ffmpeg buffering ~32KB before it writes (at 128 kbps that alone is ~2 s), the fan-out flushes per chunk, and `TCP_NODELAY` is set so Nagle can't sit on a frame.

A new encoder is primed with a rolling **4 MB replay window** of recent transport stream before live data. Without it an encoder attaches at an arbitrary mid-GOP point and has to wait for the next keyframe to recover H.264 parameters — measured on a real 720 p stream, that took 14 s in one trial and simply never succeeded in another. With it, video reliably starts in a few seconds. The cost is starting up to one window behind live.

Two things that look like low-latency settings but measured *worse*, and are deliberately not used:

* **`-fflags nobuffer`** slows stream analysis and so delays first output — about 2 s worse on video, 0.6 s on audio.
* **A very small `-analyzeduration`** is not a free cap that ffmpeg exits early from; startup tracks it almost one-for-one. Too small and video fails outright with "dimensions not set", because copying H.264 into MP4 needs SPS/PPS that TS only repeats at keyframes.

The video analysis window is **3 s**, set by measurement rather than guesswork: against 23 MB of real 720 p broadcast TS (1.92 s GOP) attaching at 12 different offsets, 2 s was the hard floor (1 s failed 4/5 times, 0.5 s always) and 3 s passed everywhere. That floor is ~2 s of *content* wherever the encoder attaches, so the replay window does not move it — replay removes the intermittent total failures, it doesn't shrink the window. Audio uses 1 s.

The window does **not** have to exceed the source GOP, which is the obvious but wrong intuition. A 1080 p stream with a 4.0 s GOP starts reliably on the 3 s window — 6/6 cold starts against a real engine — because replay hands ffmpeg a keyframe in its opening bytes so it never waits one out. Shrinking the replay window would put that back in play.

`/video` also has a floor that no amount of flushing moves, and on real streams it is the **dominant** source of video latency: an fMP4 `moof` box carries its fragment's sample table, so it cannot be written until the fragment is complete, and a client attaching must wait for the next fragment boundary.

That floor is *not* simply the source GOP. Measured on a 3.4 Mbps 720 p stream with an exact 3.0 s GOP, ffmpeg emitted a fragment every **~6 s** — two GOPs, ~2.4 MB each — so a joiner waited up to 6 s for media after receiving the init segment. Budget for a small multiple of the GOP, not one.

`FRAG_DURATION_MS` is the remedy and it does work: at `1000` on that same stream, media arrived most seconds instead of in 6 s steps. The cost is that late joiners attach mid-GOP and show artifacts until the next keyframe, which is why it is off by default.

Measured end to end against a real engine across four live streams — 720 p and 1080 p, 2.6 to 6.1 Mbps, GOPs from 1.92 s to 4.0 s — with the engine pull already warm: **0.3–6.8 s** to first byte when a listener starts a new encoder, and **under a millisecond** when attaching to one already running, which thanks to fan-out is the common case. Startup tracks GOP rather than resolution: the fastest figures came from the *highest*-bitrate stream, because its GOP was shortest. On `/video` add up to one fragment interval before *media* follows the init segment. Cold start adds P2P swarm discovery, typically 10–25 s before the engine serves anything at all. Steady-state glass-to-glass latency has not been measured.

Throughput is not the constraint the copy-first design was built to avoid. Serving a 1080 p 6.1 Mbps stream to five listeners across three formats simultaneously, the three ffmpeg processes used **0.5–4.1 % CPU** — the stream-copy paths are nearly free, and only the MP3 transcode registers at all. No listener was ever evicted for falling behind.

Late joiners on `/video` are handled: the init segment (`ftyp` + `moov`) is cached and replayed to every new listener, which is then aligned to the next fragment boundary. Audio formats need none of this — ADTS and MP3 resynchronise on any byte offset.

A listener that cannot keep up is disconnected rather than allowed to stall the shared stream; players reconnect.

## Configuration

| Env var | Default | Purpose |
| --- | --- | --- |
| `ENGINE_HOST` | *(required)* | AceStream engine `host[:port]` to pull streams from |
| `LISTEN_ADDR` | `:8080` | HTTP listen address |
| `FRAG_DURATION_MS` | *(unset)* | fMP4 fragment duration. Unset means fragment on keyframes only — lowest artifacts, latency equal to the source GOP |

## Monitoring

`GET /status` returns JSON describing every active pull, the encoders hanging off it, their listeners, and the engine's own view of each swarm (peers, speeds — passed through verbatim from the engine's `stat_url`, since its shape varies by version):

```json
{
  "uptime_s": 3841,
  "summary": { "engine_pulls": 1, "encoders": 2, "listeners": 3 },
  "streams": [{
    "content_id": "…",
    "uptime_s": 402,
    "bytes_from_engine": 301989888,
    "source": { "video": "h264", "audio": "ac3" },
    "engine": { "status": "dl", "peers": 12, "speed_down": 786, "speed_up": 120 },
    "outputs": [
      { "format": "adts", "video": "drop", "audio": "transcode",
        "listeners": [{ "peer": "10.0.0.5:51234", "uptime_s": 400, "bytes_sent": 6400000 }] },
      { "format": "fmp4", "video": "copy", "audio": "transcode",
        "listeners": [{ "peer": "10.0.0.9:44100", "uptime_s": 88, "bytes_sent": 66000000 }] }
    ]
  }]
}
```

`summary.engine_pulls` staying at one while `encoders` and `listeners` climb is the fan-out doing its job. `/healthz` is there for probes.

## Deployment

Run it in the same cluster as the engine — it pulls the full muxed stream per content id. Set `ENGINE_HOST` to the engine's in-cluster address.

## Casting to Chromecast Audio

Audio-only cast targets can't decode the muxed TS — hand them this service's stream instead. Chromecast Audio plays AAC, so the default (ADTS) output works and keeps the stream-copy path: the device receives the original audio untouched. The webplayer expects this service path-routed at `/audio` on the engine host, so the Home Assistant automation should call `media_player.play_media` with `http://<host from the cast payload>/audio?id=<id>` and `media_content_type: "music"` for audio devices. If a device won't play ADTS, append `&fmt=mp3` as a fallback.

This guidance covers `/audio` only; `/video` has not been tested against a Chromecast.

## Local development

```sh
cargo test
ENGINE_HOST=my-engine:6878 cargo run

podman build -t acestream-audio .
podman run --rm -p 8080:8080 -e ENGINE_HOST=my-engine:6878 acestream-audio
curl -v 'http://127.0.0.1:8080/audio?id=<content id>' | mpv -
curl -v 'http://127.0.0.1:8080/video?id=<content id>' | mpv -
```

No AceStream engine is needed to exercise the service: point `ENGINE_HOST` at any HTTP server that returns MPEG-TS on every path. `ffmpeg -f lavfi -i testsrc2 -f lavfi -i sine -c:v libx264 -c:a ac3 -f mpegts test.ts` generates a suitable source.
