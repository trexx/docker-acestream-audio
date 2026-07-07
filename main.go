// acestream-audio: strips the video track from an AceStream engine stream and
// serves the audio over plain HTTP, so phones, cars and Chromecast Audios can
// play it with a fraction of the data of the full muxed stream.
//
//	GET /audio?id=<40-hex content id>[&fmt=adts|mp3]
//
// Copy-first: when the source audio is already decodable by browsers (AAC for
// fmt=adts, MP3 for fmt=mp3) it is stream-copied untouched — no re-encode, no
// quality loss, near-zero CPU. Other codecs (AC3/E-AC3/DTS/MP2) are transcoded.
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/exec"
	"regexp"
	"strings"
	"time"
)

// -rw_timeout in microseconds: give up if the engine stops sending data.
const engineStallTimeout = "30000000"

var (
	idRe       = regexp.MustCompile(`^[0-9a-fA-F]{40}$`)
	engineHost = os.Getenv("ENGINE_HOST")
)

func main() {
	if engineHost == "" {
		log.Fatal("ENGINE_HOST must be set")
	}
	addr := envOr("LISTEN_ADDR", ":8080")
	mux := http.NewServeMux()
	mux.HandleFunc("/audio", audio)
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) { fmt.Fprintln(w, "ok") })
	mux.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprintln(w, "usage: GET /audio?id=<40-hex content id>[&fmt=adts|mp3]")
	})
	// WriteTimeout must stay 0: /audio responses stream for hours.
	srv := &http.Server{Addr: addr, Handler: mux, ReadHeaderTimeout: 10 * time.Second}
	log.Printf("listening on %s (engine host: %s)", addr, engineHost)
	log.Fatal(srv.ListenAndServe())
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func audio(w http.ResponseWriter, r *http.Request) {
	q := r.URL.Query()
	id := q.Get("id")
	outFmt := q.Get("fmt")
	if outFmt == "" {
		outFmt = "adts"
	}

	switch {
	case !idRe.MatchString(id):
		http.Error(w, "id must be a 40-char hex content id", http.StatusBadRequest)
		return
	case outFmt != "adts" && outFmt != "mp3":
		http.Error(w, "fmt must be adts or mp3", http.StatusBadRequest)
		return
	}

	// Distinct player id (pid) per request: the engine treats getstream requests
	// without distinct pids as the same player session and stops serving the old
	// one when a new request arrives — without this, /audio would fight a browser
	// playing the same stream directly. Probe and ffmpeg deliberately share the
	// pid: the probe connection is closed before ffmpeg starts, and reusing it
	// lets ffmpeg take over that engine session instead of leaving it dangling.
	src := fmt.Sprintf("http://%s/ace/getstream?id=%s&pid=audio-%x", engineHost, id, time.Now().UnixNano())

	codec, err := probeAudioCodec(r.Context(), src)
	if err != nil {
		log.Printf("%s: probe %s failed: %v", r.RemoteAddr, src, err)
		http.Error(w, "could not probe stream audio: "+err.Error(), http.StatusBadGateway)
		return
	}

	var codecArgs []string
	mode := "copy"
	contentType := "audio/aac"
	switch {
	case outFmt == "adts" && codec == "aac":
		codecArgs = []string{"-c:a", "copy"}
	case outFmt == "adts":
		mode = "transcode"
		codecArgs = []string{"-c:a", "aac", "-b:a", "128k", "-ac", "2"}
	case outFmt == "mp3" && codec == "mp3":
		contentType = "audio/mpeg"
		codecArgs = []string{"-c:a", "copy"}
	default:
		mode = "transcode"
		contentType = "audio/mpeg"
		codecArgs = []string{"-c:a", "libmp3lame", "-b:a", "128k", "-ac", "2"}
	}

	args := []string{
		"-hide_banner", "-loglevel", "error",
		"-rw_timeout", engineStallTimeout,
		"-i", src,
		"-vn", "-sn", "-dn",
	}
	args = append(args, codecArgs...)
	args = append(args, "-f", outFmt, "pipe:1")

	// r.Context() is canceled when the client goes away, which kills ffmpeg —
	// no orphaned transcodes.
	cmd := exec.CommandContext(r.Context(), "ffmpeg", args...)
	cmd.Stderr = os.Stderr
	stdout, err := cmd.StdoutPipe()
	if err == nil {
		err = cmd.Start()
	}
	if err != nil {
		log.Printf("%s: ffmpeg start failed: %v", r.RemoteAddr, err)
		http.Error(w, "ffmpeg start failed", http.StatusInternalServerError)
		return
	}
	defer func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	}()

	log.Printf("%s: %s %s audio as %s from %s", r.RemoteAddr, mode, codec, outFmt, src)
	start := time.Now()

	w.Header().Set("Content-Type", contentType)
	w.Header().Set("Cache-Control", "no-store")
	w.WriteHeader(http.StatusOK)

	rc := http.NewResponseController(w)
	var sent int64
	buf := make([]byte, 32*1024)
	for {
		n, rerr := stdout.Read(buf)
		if n > 0 {
			if _, werr := w.Write(buf[:n]); werr != nil {
				break
			}
			sent += int64(n)
			_ = rc.Flush()
		}
		if rerr != nil {
			break
		}
	}
	log.Printf("%s: stream ended after %s, %d bytes sent", r.RemoteAddr, time.Since(start).Round(time.Second), sent)
}

// probeAudioCodec returns the codec name of the first audio stream (e.g. "aac", "ac3").
func probeAudioCodec(ctx context.Context, src string) (string, error) {
	ctx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	out, err := exec.CommandContext(ctx, "ffprobe",
		"-v", "error",
		"-select_streams", "a:0",
		"-show_entries", "stream=codec_name",
		"-of", "default=nw=1:nk=1",
		src).Output()
	if err != nil {
		var exitErr *exec.ExitError
		if errors.As(err, &exitErr) && len(exitErr.Stderr) > 0 {
			return "", errors.New(strings.TrimSpace(string(exitErr.Stderr)))
		}
		return "", err
	}
	codec := strings.TrimSpace(string(out))
	if codec == "" {
		return "", errors.New("no audio stream found")
	}
	// MPEG-TS sources list the stream both inside the program and at top level,
	// so ffprobe prints codec_name twice — take the first.
	return strings.Fields(codec)[0], nil
}
