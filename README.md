# Mealie Forager

Turns recipe posts from TikTok, Instagram, YouTube, and similar sites (or any recipe
website Mealie can scrape) into clean, structured Mealie recipes. It's a Rust (axum + SQLite) service with a persistent job queue and a live
web UI. The UI is plain HTML, CSS, and JS embedded in the binary.

## Quick start (Docker)

You need a running [Mealie](https://mealie.io) (3.28+ for photo and text imports) and an
OpenAI API key, or a key for any OpenAI-compatible API that offers chat and
transcription models.

1. In Mealie, open your profile → **API Tokens** and generate a token.
2. Download [`docker-compose.yml`](docker-compose.yml) and
   [`.env.example`](.env.example) into a folder, rename `.env.example` to `.env`,
   and fill in `MEALIE_URL`, `MEALIE_API_KEY`, `OPENAI_API_KEY`, and
   `AUTH_PASSWORD`.
3. Run `docker compose up -d` and open `http://<host>:3000`. Log in with any
   username and your `AUTH_PASSWORD`.

If Mealie runs in the same compose project, add Forager as another service and
point `MEALIE_URL` at Mealie's service name (e.g. `http://mealie:9000`). Also set
`MEALIE_PUBLIC_URL` to the address you open Mealie at, so links in the UI work.

The image (`ghcr.io/davisschenk/mealie-forager`, amd64 and arm64) bundles
`yt-dlp`, `gallery-dl`, `ffmpeg`, and Deno (which `yt-dlp` needs for YouTube).
Its data (the job database and uploads) lives in `/data`. Sites change often and
`yt-dlp` has to keep up, so pull a newer image if downloads start failing.

Every job costs a few OpenAI calls: a transcription for videos and one or more
chat requests to extract and clean the recipe. Keep an eye on your OpenAI usage
when you first start.

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

**Recipes already in Mealie** can be cleaned too. Paste a Mealie recipe link into
the import box, or send it through the API or Shortcut, and Forager queues a
clean-only job instead of importing it again. The "Mealie library" panel counts
the recipes without the clean tag, and one button queues a clean for all of
them. The API equivalents are `POST /api/clean` (`{"slug": …}` or `{"url": …}`)
and `GET`/`POST /api/clean/library`. Clean-only jobs wait behind new imports.

Imports only add the tags you give the job. A website's SEO keywords and the
model's keywords aren't turned into tags, since categories cover the same ground.

Anything the model thinks needs a human look shows up as a warning in the job log.
If the cleanup fails, the recipe stays in Mealie without the tag. Retrying the job
then re-runs only the cleanup, not the import. Retrying a job that already
succeeded cleans its recipe again.

Posts without audio skip Download and Transcribe. A retry reuses the transcript and
recipe saved by earlier attempts, so a failed import doesn't pay for OpenAI calls
again. "Retry from scratch" discards that saved work, including the link to the Mealie recipe. If the service stops while a
job is running, the job goes back into the queue on the next start.

## API and iOS Shortcut

`POST /api/jobs/upload` queues an import. Send one or more `file` fields (photos,
text, a Mealie `.zip`, or a video/audio file), or a `url` or `text` field. The
optional fields are `tags` (comma-separated or repeated), `note`, `source`, and
`force`. The body can be a multipart form, a URL-encoded form, or JSON. It can
also be a raw file as the whole body, with the optional fields in the query
string.

`POST /api/jobs` queues a link from a JSON body with `url`, plus optional `tags`,
`note`, and `source`.

Both endpoints respond `201` with the job, or `409` if the link was already
imported. Send `force` as true to import it again.

API clients authenticate with `Authorization: Bearer <token>`. The token is
generated on first start and stored in the database. The web UI's "iOS Shortcut"
panel shows it and can regenerate it. A request with a wrong token gets a `401`.

## Security

Anyone who can open the UI can read the API token and queue jobs that spend your
OpenAI credits, so don't expose it unprotected. There are two options:

- Set `AUTH_PASSWORD`. The browser then asks for a login (any username, that
  password) on every page and API call, except `/healthz` and the app icons.
  Bearer-token requests skip the login, so the Shortcut keeps working.
- Leave `AUTH_PASSWORD` unset and put Forager behind a reverse proxy that
  authenticates (Authelia, Authentik, Cloudflare Access, …). Without a
  password, requests with no `Authorization` header aren't checked. The proxy can
  let requests that carry a Bearer header through to `/api/jobs` without its
  login.

Use HTTPS (from your proxy or tunnel) whenever Forager is reachable from outside
your network, since Basic auth and the token are otherwise sent in the clear.

For an iOS Shortcut that shows up in the share sheet (accepting URLs, Text,
Images, Media, and Files), add **Get Contents of URL** to
`https://<host>/api/jobs/upload` with method `POST`, the `Authorization` header,
and a Form body. Shortcuts can't send a link in a File field, so branch on
**Get URLs from Input**. If it finds URLs, send them in a Text field named `url`.
Otherwise, send Shortcut Input in a File field named `file`. That one shortcut
handles links, photos, screenshots, and videos.

## Configuration

Settings come from environment variables ([`.env.example`](.env.example) lists them with comments):

| Variable | Default |
| --- | --- |
| `OPENAI_API_KEY`, `MEALIE_API_KEY`, `MEALIE_URL` | required |
| `AUTH_PASSWORD` | unset (no built-in login; see [Security](#security)) |
| `MEALIE_PUBLIC_URL` | `MEALIE_URL` (used for links in the UI) |
| `MEALIE_GROUP_NAME` | `home` |
| `OPENAI_URL` | `https://api.openai.com/v1` |
| `TRANSCRIPTION_MODEL` / `TEXT_MODEL` | `whisper-1` / `gpt-5-mini` |
| `EXTRA_PROMPT` | extra instructions for every extraction and cleanup |
| `CLEANUP` | `true` (set `false` to skip the Clean stage) |
| `CLEAN_MODEL` | `TEXT_MODEL` |
| `CLEAN_TAG` | `Imported Clean` |
| `LISTEN_ADDR` | `127.0.0.1:3000` (`0.0.0.0:3000` in Docker) |
| `DATABASE_PATH` | `mealie-forager.db` (`/data/mealie-forager.db` in Docker) |
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
    # OPENAI_API_KEY, MEALIE_API_KEY, and optionally AUTH_PASSWORD
    environmentFile = "/run/secrets/mealie-forager.env";
  };
}
```

The service runs as a `DynamicUser` and keeps its database in
`/var/lib/private/mealie-forager`. Set `AUTH_PASSWORD` in the environment file or
put the UI behind your own authentication (see [Security](#security)).

## Development

```sh
nix develop      # cargo, clippy, yt-dlp, ffmpeg, gallery-dl
cargo test
OPENAI_API_KEY=… MEALIE_API_KEY=… MEALIE_URL=http://localhost:9000 cargo run
nix flake check  # builds the package, which also runs the tests
docker build -t mealie-forager .
```

## Contributing

Bug reports and pull requests are welcome. Please run `cargo fmt`, `cargo clippy`
and `cargo test` before opening a PR. See [CHANGELOG.md](CHANGELOG.md) for what
has changed between versions.

## License

[MIT](LICENSE)
