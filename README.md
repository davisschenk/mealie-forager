# Mealie Forager

Turns recipe posts from TikTok, Instagram, YouTube, and similar sites (or any recipe
website Mealie can scrape) into clean, structured Mealie recipes. It's a Rust (axum + SQLite) service with a persistent job queue and a live
web UI. The UI is plain HTML, CSS, and JS embedded in the binary.

## Pipeline

Each job has a source. By default it is picked from the link: posts from social
sites (TikTok, Instagram, YouTube, Facebook, Pinterest, X, Reddit, …) take the
**social** path, and every other link takes the **web** path. The web UI and the
`source` field of `POST /api/jobs` (`auto`, `social`, or `web`) can override that.

**Web** jobs go straight to Import, where Mealie's own scraper
(`/api/recipes/create/url`) imports the page. If Mealie can't scrape it, the job
falls back to the social path.

**Social** jobs run through these stages. The queue records how long each stage took.

| Stage      | What happens                                                                 |
| ---------- | ---------------------------------------------------------------------------- |
| Fetch      | `yt-dlp --dump-single-json` reads the caption and metadata. If that fails, `gallery-dl` handles image posts. |
| Download   | `yt-dlp` downloads the best audio track (with live progress), and `ffmpeg` re-encodes it to 16 kHz mono MP3. |
| Transcribe | The OpenAI `/audio/transcriptions` endpoint transcribes the audio.           |
| Extract    | `/chat/completions` with a strict JSON schema returns the recipe, or reports that the post has none. |
| Import     | The recipe is sent to Mealie's `/api/recipes/create/html-or-json` as schema.org JSON-LD. |

**Uploads** (`POST /api/jobs/upload`, or attach/drop files in the UI) go by file type:

| Upload | Handled by |
| --- | --- |
| Photos and screenshots (JPEG, PNG, HEIC, WebP, …), text/HTML/JSON files, pasted recipe text | Mealie's AI import (`/api/recipes/create/ai`, Mealie 3.28+ with OpenAI enabled). The first photo becomes the cover. |
| A recipe exported from Mealie (`.zip`) | Mealie's zip import |
| A video or voice memo (MP4, MOV, M4A, MP3, …) | ffmpeg, then Transcribe, Extract, and Import as above |

A link sent as a file or text field becomes a normal link job. PDFs aren't
supported. Uploaded files are kept under `UPLOAD_DIR` for retries until the job is
removed.

After any of these paths, a **Clean** stage tidies the recipe in Mealie:

- Every ingredient becomes a structured line with a quantity, a unit, a food, and a
  note. Foods are linked to the cleanest existing Mealie food. An exact name match
  wins. Otherwise the model chooses among search results, and a new food is created
  only when nothing fits. Units come from Mealie's list, and duplicates named after
  another unit's abbreviation (such as a `tbsp` unit next to `tablespoon`) are
  never used.
- Alternatives ("chicken broth or vegetable broth") become Mealie ingredient
  substitutions, linked to a food when they name one. Substitutions already on a
  line are kept. Substitutions the food already has at the food level aren't
  repeated on the recipe. Food and unit aliases count as exact matches.
- Steps are made imperative, one action each, and linked to their ingredients.
  Creator chatter is removed.
- The name, description, yield, servings, and times are tidied.
- The recipe is given one to three of your existing Mealie categories (meal type,
  course, cuisine, whatever your list covers). Categories are never created, and
  ones the recipe already has are kept.
- Hashtag tags are removed and unused hashtag tags are deleted. Last of all, the
  recipe gets the `Imported Clean` tag.

Anything the model thinks needs a human look shows up as a warning in the job log.
If the cleanup fails, the recipe stays in Mealie without the tag. Retrying the job
then re-runs only the cleanup, not the import. Retrying a job that already
succeeded cleans its recipe again.

Posts without audio skip Download and Transcribe. A retry reuses the transcript and
recipe saved by earlier attempts, so a failed import doesn't pay for OpenAI calls
again. "Retry from scratch" discards that saved work, including the link to the Mealie recipe. If the service stops while a
job is running, the job goes back into the queue on the next start.

## API and iOS Shortcut

`POST /api/jobs/upload` takes a multipart form. Send one or more `file` fields
(photos, text, a Mealie `.zip`, or a video/audio file), or a `url` or `text`
field. The optional fields are `tags` (comma-separated or repeated), `note`,
`source`, and `force`. `POST /api/jobs` with a JSON body queues a link. `url` is required, and `tags`,
`note`, and `source` are optional. The response is `201` with the job, or `409` if
the link was already imported (send `"force": true` to import it again).

API clients authenticate with `Authorization: Bearer <token>`. The token is
generated on first start and stored in the database. The web UI's "iOS Shortcut"
panel shows it and can regenerate it. A request with a wrong token gets a `401`.
Requests without an `Authorization` header aren't checked, so keep the UI behind a
proxy that authenticates them. The proxy can let requests that carry a Bearer
header through to `/api/jobs` without its login.

For an iOS Shortcut that shows up in the share sheet (accepting URLs, Text,
Images, Media, and Files), add **Get Contents of URL** to
`https://<host>/api/jobs/upload` with method `POST`, the `Authorization` header,
and a Form body with a File field named `file` set to Shortcut Input. The same
shortcut handles links, photos, screenshots, and videos.

## Configuration

Settings come from environment variables:

| Variable | Default |
| --- | --- |
| `OPENAI_API_KEY`, `MEALIE_API_KEY`, `MEALIE_URL` | required |
| `MEALIE_PUBLIC_URL` | `MEALIE_URL` (used for links in the UI) |
| `MEALIE_GROUP_NAME` | `home` |
| `OPENAI_URL` | `https://api.openai.com/v1` |
| `TRANSCRIPTION_MODEL` / `TEXT_MODEL` | `whisper-1` / `gpt-5-mini` |
| `EXTRA_PROMPT` | extra instructions for every extraction and cleanup |
| `CLEANUP` | `true` (set `false` to skip the Clean stage) |
| `CLEAN_MODEL` | `TEXT_MODEL` |
| `CLEAN_TAG` | `Imported Clean` |
| `LISTEN_ADDR` | `127.0.0.1:3000` |
| `DATABASE_PATH` | `mealie-forager.db` |
| `UPLOAD_DIR` | `uploads/` next to the database |
| `MAX_UPLOAD_MB` | `100` (Cloudflare Tunnel's request limit) |
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
none of its own apart from the API token described above.

## Development

```sh
nix develop      # cargo, clippy, yt-dlp, ffmpeg, gallery-dl
cargo test
OPENAI_API_KEY=… MEALIE_API_KEY=… MEALIE_URL=http://localhost:9000 cargo run
nix flake check  # builds the package, which also runs the tests
```
