# Mealie Forager

Turns recipe posts from TikTok, Instagram, YouTube, and similar sites into Mealie
recipes. It's a Rust (axum + SQLite) service with a persistent job queue and a live
web UI. The UI is plain HTML, CSS, and JS embedded in the binary.

## Pipeline

Each job runs through these stages. The queue records how long each stage took.

| Stage      | What happens                                                                 |
| ---------- | ---------------------------------------------------------------------------- |
| Fetch      | `yt-dlp --dump-single-json` reads the caption and metadata. If that fails, `gallery-dl` handles image posts. |
| Download   | `yt-dlp` downloads the best audio track (with live progress), and `ffmpeg` re-encodes it to 16 kHz mono MP3. |
| Transcribe | The OpenAI `/audio/transcriptions` endpoint transcribes the audio.           |
| Extract    | `/chat/completions` with a strict JSON schema returns the recipe, or reports that the post has none. |
| Import     | The recipe is sent to Mealie's `/api/recipes/create/html-or-json` as schema.org JSON-LD. |

Posts without audio skip Download and Transcribe. A retry reuses the transcript and
recipe saved by earlier attempts, so a failed import doesn't pay for OpenAI calls
again. "Retry from scratch" discards that saved work. If the service stops while a
job is running, the job goes back into the queue on the next start.

## Configuration

Settings come from environment variables:

| Variable | Default |
| --- | --- |
| `OPENAI_API_KEY`, `MEALIE_API_KEY`, `MEALIE_URL` | required |
| `MEALIE_PUBLIC_URL` | `MEALIE_URL` (used for links in the UI) |
| `MEALIE_GROUP_NAME` | `home` |
| `OPENAI_URL` | `https://api.openai.com/v1` |
| `TRANSCRIPTION_MODEL` / `TEXT_MODEL` | `whisper-1` / `gpt-5-mini` |
| `EXTRA_PROMPT` | extra instructions for every extraction |
| `LISTEN_ADDR` | `127.0.0.1:3000` |
| `DATABASE_PATH` | `mealie-forager.db` |
| `WORK_DIR` | system temp dir |
| `WORKERS` | `2` |
| `MAX_DURATION_SECS` | `1800` |
| `COOKIES_FILE` | Netscape cookie jar for `yt-dlp` and `gallery-dl` (optional) |
| `YTDLP_PATH`, `FFMPEG_PATH`, `GALLERY_DL_PATH` | looked up on `PATH` |

## NixOS

```nix
{
  inputs.mealie-forager.url = "github:davisschenk/mealie-forager";

  # in a NixOS configuration:
  imports = [ inputs.mealie-forager.nixosModules.default ];
  services.mealie-forager = {
    enable = true;
    port = 4000;
    mealieUrl = "http://127.0.0.1:9000";
    mealiePublicUrl = "https://mealie.example.com";
    environmentFile = "/run/secrets/mealie-forager.env"; # OPENAI_API_KEY, MEALIE_API_KEY
  };
}
```

The service runs as a `DynamicUser` and keeps its database in
`/var/lib/private/mealie-forager`. Put the UI behind your own authentication; it has
none of its own.

## Development

```sh
nix develop      # cargo, clippy, yt-dlp, ffmpeg, gallery-dl
cargo test
OPENAI_API_KEY=… MEALIE_API_KEY=… MEALIE_URL=http://localhost:9000 cargo run
nix flake check  # builds the package, which also runs the tests
```
