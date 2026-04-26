# PulseAudio Play Forward

`pafd` is a small client/server tool for forwarding audio bytes over HTTP to a
host running `paplay` (PulseAudio / PipeWire). The motivating use case is
letting a Docker container play sound on the host without mounting the
PulseAudio socket into the container.

It also bundles a text-to-speech (TTS) client and a small library of canned
sound effects, so the same binary can synthesize speech, play a built-in sound,
or stream arbitrary audio bytes to the playback daemon.

## How it works

- `pafd server` runs on the host and listens on an HTTP port. It accepts a raw
  audio stream on `POST /stream` and pipes the bytes to `paplay`.
- `pafd tts` and `pafd sound` run as clients inside a container. They produce
  audio bytes (synthesized speech or a bundled OGG) and stream them to the
  server, which plays them through the host's PulseAudio sink.
- Optional bearer-token auth (`--token` / `PAFD_TOKEN`) protects the playback
  endpoint.

## Installation

Install the `pafd` binary directly from the git repository with `cargo install`:

```bash
cargo install --locked --force --git https://github.com/andrewbrey/pafd
```

The host running `pafd server` needs `paplay` on `PATH` (typically provided by
`pulseaudio-utils` or PipeWire's pulse shim).

## Usage

### Server (host)

```bash
# Bind defaults to 0.0.0.0:8421
pafd server

# Bind elsewhere and require a bearer token
PAFD_TOKEN=secret pafd --token secret server --bind 0.0.0.0:8421
```

The server:

- Accepts `POST /stream` with a raw audio body (anything `paplay` can decode
  from stdin: WAV, OGG, raw PCM, etc.).
- Enforces a 64 MiB body limit and a 60 s per-stream wall-clock cap.

#### Running as a systemd user service

A hardened unit file is provided at [`pafd.service`](pafd.service). It runs as a
systemd **user** service so it inherits your login session's audio sockets
without extra plumbing.

```bash
mkdir -p ~/.config/systemd/user
cp pafd.service ~/.config/systemd/user/pafd.service
systemctl --user daemon-reload
systemctl --user enable --now pafd.service

# Logs:
journalctl --user -u pafd -f
```

To use a bearer token, edit the unit file as needed.

### TTS client

```bash
# Speak a message via the configured server
pafd --token secret tts --server http://host:8421 "hello world"

# Read text from a file
pafd tts --server http://host:8421 --input message.txt

# List available providers and their voices
pafd tts --info
```

Flags:

- `--provider {auto,edge-tts,google-tts,piper}` — pick a synthesis backend.
  `auto` (default) tries providers in order and uses the first one that works.
- `--voice <id>` — preferred provider voice id.
- `--info` — list providers and voices, then exit.

#### TTS providers

- **edge-tts** — Microsoft Edge's online TTS service over WebSocket. No install
  required; needs network access.
- **google-tts** — Google Translate's TTS endpoint. No install required; needs
  network access.
- **piper** — local [piper](https://github.com/rhasspy/piper) neural TTS.
  Requires the `piper` binary on `PATH`; binary and voice models are downloaded
  and cached on first use if not already present.

### Sound client

`pafd` ships with a set of bundled OGG sound effects useful for shell scripts
and agent feedback (delivery confirmations, item pickups, etc.).

```bash
# List bundled sounds
pafd sound --list

# Play a bundled sound on the server
pafd --token secret sound --server http://host:8421 confirm_delivery
```

Bundled sounds: `animal_stick`, `b2`, `been_tree`, `complete_quest_requirement`,
`confirm_delivery`, `flitterbug`, `here_you_go_lighter`, `hi_flowers_hit`,
`item_pickup`, `save_and_checkout`.

## Configuration

All client commands accept the playback server URL and bearer token via flags or
environment variables:

| Flag              | Env var       | Purpose                    |
| ----------------- | ------------- | -------------------------- |
| `--server <URL>`  | `PAFD_SERVER` | Playback server base URL   |
| `--token <TOKEN>` | `PAFD_TOKEN`  | Bearer token for `/stream` |

`RUST_LOG` controls log verbosity (via `tracing-subscriber` env-filter).

## HTTP API

Single endpoint, intended to be called by `pafd` clients but usable by anything
that can stream bytes:

```
POST /stream
Authorization: Bearer <token>   # if server started with --token
Content-Type: <audio mime>      # e.g. audio/ogg, audio/wav, audio/L16
<raw audio bytes>
```

- `200 ok` on successful playback.
- `400` on empty body or stream errors.
- `401` on missing/invalid token.
- `503` if the server is shutting down mid-stream.

Body is capped at 64 MiB; total stream duration is capped at 60 s.

## Acknowledgments

The text-to-speech synthesis is inspired by
[`voipi`](https://github.com/pithings/voipi).

## License

MIT. See [LICENSE](LICENSE).
