# ArtCraft Native

ArtCraft's desktop app in pure Rust: an [egui](https://github.com/emilk/egui) UI over the same
ArtCraft API client crates the Tauri app uses. There is no Tauri and no JavaScript; the only
webview is the Midjourney sign-in window, which exists to capture Midjourney's login cookies.

The look follows the brutalist webapp (`artcraft-services/frontend/apps/artcraft-webapp`). That
means near-black surfaces with white/15 hairlines and 3 px corners on controls. Type is Archivo for
display, Inter for body text and Geist Mono for labels. The brand blue marks the primary actions.

Create Image, Create Video and Create Audio are fully built. The other tools show a "coming soon"
page that links to the same tool in the web app.

## Running

```bash
cargo run -p artcraft_native --release
```

The workspace's default member is the Tauri app, so pass `-p artcraft_native`.

- **Sign-in.** On first launch the app reuses an existing ArtCraft login. It reads the session
  cookie from the Tauri app's jar (`<cache dir>/ai.artcraft.app/.cookies`) and keeps its own copy in
  `~/Artcraft/credentials/native_session.json`. Otherwise use "Sign in with browser" (device code)
  or a username and password.
- **API host.** `~/Artcraft/settings/env_configs.json` selects it, exactly as for the Tauri app.

## Local mode (your own cloud accounts)

This fork runs in **local mode** by default: no ArtCraft sign-in, no credits, and no calls to
ArtCraft's servers. Generations bill your own Google Cloud and AWS accounts, and the library lives
on disk in `~/Artcraft/local/`. Set `ARTCRAFT_REMOTE=1` to get the stock ArtCraft account flow back.

Configure it in `~/Artcraft/settings/local_providers.json` (written on first launch), or with
environment variables, which win over the file:

```json
{
  "gcp_project": "my-project",
  "gcp_location": "global",
  "gcp_video_location": "us-central1",
  "aws_region": "us-west-2",
  "aws_profile": "default",
  "jev_api_key": "ts_...",
  "jev_min_confidence": 0.35
}
```

| Setting           | Environment variable                            | Notes                                                       |
|-------------------|-------------------------------------------------|-------------------------------------------------------------|
| `gcp_project`     | `ARTCRAFT_GCP_PROJECT` / `GOOGLE_CLOUD_PROJECT` | Vertex AI; auth via `gcloud auth application-default login` |
| `gcp_api_key`     | `ARTCRAFT_GCP_API_KEY`                          | Express-mode key instead of a project; images only          |
| `bedrock_enabled` | —                                               | Default: on when AWS credentials are found                  |
| `aws_region`      | `ARTCRAFT_AWS_REGION`                           | Stability models are in `us-west-2`                         |
| `aws_profile`     | `AWS_PROFILE`                                   | Any SDK credential source works (SSO, env, roles)           |
| `jev_api_key`     | `ARTCRAFT_JEV_API_KEY` / `TYPESAFE_API_KEY`     | Turns on Jev routing for the Auto models                    |

Models offered (only for the accounts that are set up):

| Model              | Provider | Upstream ID                         |
|--------------------|----------|-------------------------------------|
| Nano Banana 2      | Vertex   | `gemini-3.1-flash-image`            |
| Nano Banana Pro    | Vertex   | `gemini-3-pro-image`                |
| Nano Banana 2 Lite | Vertex   | `gemini-3.1-flash-lite-image`       |
| Stable Image Ultra | Bedrock  | `stability.stable-image-ultra-v1:1` |
| SD 3.5 Large       | Bedrock  | `stability.sd3-5-large-v1:0`        |
| Stable Image Core  | Bedrock  | `stability.stable-image-core-v1:1`  |
| Veo 3.1 Fast       | Vertex   | `veo-3.1-fast-generate-001`         |
| Veo 3.1            | Vertex   | `veo-3.1-generate-001`              |
| Veo 3.1 Lite       | Vertex   | `veo-3.1-lite-generate-001`         |

**Auto** (image and video) routes each request. Models that can't honor the settings (aspect
ratio, resolution, reference count, frames, duration) are ruled out first. Jev then picks among
the rest from the prompt and settings; below `jev_min_confidence`, or without a Jev key, the first
fitting model in the order above is used (Nano Banana Pro when there are more than three
references). A toast says which model ran and why.

**Spend meter.** The top bar shows today's spend on your accounts next to the profile icon; click
it for today and this month by account (Google Cloud, AWS Bedrock, Jev). The app meters each call
itself, because neither cloud reports spend in real time: Gemini by the tokens the reply reports,
Veo by seconds of video, Stability per image, Jev by input tokens, all at October 2026 list prices
(`src/backend/local/pricing.rs`). Override a rate in `local_providers.json` under `"prices"`, e.g.
`{ "stability.sd3-5-large-v1:0": 0.065 }`. The ledger is `~/Artcraft/local/spend.json`.

**Cost before you generate.** Beside the generate button, the prompt box shows what the request
would cost at list prices: one figure for a chosen model, or for Auto the range across the models
that fit (hover for each model's price and the rules' likely pick). It updates as you change the
model, size, count, duration or audio, and asks no API, so it costs nothing.

**Routing panel.** The lightbox's side column shows how each local generation was made: what was
requested, who decided (Jev with its confidence, the rules, or you), the model and API model that
ran, Jev's odds for every option against the confidence bar, which models were ruled out and why,
the cost and how long it took.

**Tools.** In local mode the Studio pages work too. Each one has a stage, an options panel with the
cost, and a strip of results that also land in the library; any local image or video in the lightbox
has "Open in" buttons for them.

| Page          | What it does                                                     | Runs on                     |
|---------------|------------------------------------------------------------------|-----------------------------|
| Edit Image    | Describe a change (optional painted mask); inpaint, erase, replace, expand | Vertex (Nano Banana); Bedrock (Stability) |
| BG Remove     | Transparent cut-out                                              | Your Mac (ISNet), or Bedrock |
| Frame Extract | Marked moments or one frame every N seconds                      | Your Mac (FFmpeg)           |
| Edit Video    | Trim, join, extend about 7 s                                     | Your Mac (FFmpeg); Vertex (Veo) |
| 3D Stage      | Block a shot with people, shapes and imported GLB/OBJ, then paint it | Your Mac; Vertex for Generate |
| Audio         | 30-second instrumental clips                                     | Vertex (Lyria 2)            |

FFmpeg (`brew install ffmpeg`) is needed for video previews, Frame Extract and Edit Video. 3D Object
and 3D World still say "coming soon": they need a 3D generation provider.

Not in local mode yet: audio models, characters, cost estimates, and Bedrock video (Amazon Nova
Reel was retired on 2026-09-30; Luma Ray2 needs an S3 bucket). The code is in
`src/backend/local/`.

## Architecture

| Module             | What it does                                                                   |
|--------------------|--------------------------------------------------------------------------------|
| `backend`          | Tokio runtime and every API call; results come back to the UI as `Event`s      |
| `backend::wire`    | Lenient response shapes; new server enum values can't break decoding           |
| `backend::session` | Credentials: own file, Tauri cookie import, env config                         |
| `backend::media_cache` | Thumbnails and previews: downloaded and decoded off-thread, evicted when unused |
| `backend::audio`   | One clip at a time: reference previews, feed tracks (pause, seek, volume), waveforms |
| `backend::providers` | Midjourney and fal: accounts, submitting, and a worker that finishes jobs into the library |
| `models`           | Model catalog from OmniGen listings, plus names, families and limits           |
| `prompt_box`       | The shared prompt box: editor, reference deck, keyframes, pickers, model selector |
| `pages`            | Create Image, Video and Audio, Home, Library, and the shared page shell        |
| `feed`             | Generation feed: running, failed and finished jobs, library paging, grid and list |
| `overlays`         | Lightbox, library picker, sign-in, settings, provider setup and the Midjourney sign-in window |
| `shell`            | Custom title bar, sidebar flush to the window edge, content panel header       |
| `theme`, `ui`      | Tokens, Lucide-style icons, widgets, toasts, window chrome (drag, resize, caption buttons) |

How the backend works:

- **Generation.** It goes through OmniGen: `/v1/omni_gen/{models,cost,generate}/{image,video,audio}`.
  Model capabilities come from the server, so new models and options appear without an app update.
- **Typed client functions.** Model lists, uploads, credits, login and deletes use
  `artcraft_client`.
- **Lenient decoding.** Generate and cost requests are JSON maps, as in the Tauri app. Job polling
  (`/v1/jobs/session`), the audio model list, the library list, batches and prompts are decoded
  with lenient structs.
- **Identity headers.** Every request sends the desktop identity headers: `Origin` and
  `User-Agent`.
- **Other providers.** Images can also run on Midjourney (your own account) or fal (your own API
  key), through `artcraft_router` like the Tauri app. The accounts live in the same files
  (`~/Artcraft/credentials`), so connecting in either app works in both. Those jobs are tracked in
  `~/Artcraft/state/native_provider_jobs.json` and their results uploaded into your library, resuming
  after a restart.

## Prompt box parity

| Feature                                                         | Status                                  |
|-----------------------------------------------------------------|-----------------------------------------|
| Auto-growing prompt (~4 lines), expand/collapse, resize grip    | Done                                    |
| Focus mode (fullscreen editor with deck and toolbar)            | Done                                    |
| Length counter, red over the model's limit                      | Done                                    |
| Enter to generate (setting; Shift+Enter for a new line)         | Done                                    |
| `@Image1` / `@Video1` / `@Audio1` highlight and autocomplete    | Done                                    |
| Reference deck: fan, +N badge, hover-expand, reorder, clear all | Done                                    |
| Keyframe slots: first/last frame, tilt, swap                    | Done                                    |
| Upload, pick from library, drag & drop, paste image             | Done (video uploads must be MP4)        |
| Model selector with families, logos, capability badges          | Done                                    |
| Aspect ratio, resolution, quality, count                        | Done                                    |
| Bitrate, output format, duration slider, sound, input mode      | Done                                    |
| Live credit cost on the generate button                         | Done                                    |
| Validation: starting frame, text-only banner, limits, uploads   | Done                                    |
| Clear all (confirms when references are attached)               | Done                                    |
| `@Character` mentions and the Characters modal (create/edit/delete) | Done                                |
| Provider choice (Midjourney direct, fal), "Select Provider"     | Done (images; video runs on ArtCraft)   |
| In-app video playback                                           | Done with FFmpeg on the PATH; otherwise opens in the system player |
| Audio reference playback (click to play/stop)                   | Done                                    |
| Animated video previews in the feed (with a stills toggle)      | Done                                    |
| Audio: style, instrumental, keep lyrics, loop                   | Done                                    |
| Audio: Beat & Key (BPM, musical key) and Tuning (sample rate, speed, volume, pitch) | Done                |
| Audio references (play, 600 s cap) or one image, never both     | Done                                    |

Feed and lightbox:

- **Feed.** Pending cards show progress, time left and a batch banner. Failed cards show the reason
  and can be dismissed. The grid is masonry, with a list view alongside. Hover actions are Recreate,
  Make Video, Share and Download. Select mode does batch downloads. The last viewed item is marked.
- **Audio.** Audio cards play in place: a waveform (click to seek), the time and a volume flyout.
  One track plays at a time and stops when you leave the page. Audio offers Share and Download.
- **Lightbox.** Prev/next with the arrow keys, prompt copy and details. Actions are Recreate, Make
  Video, Download, Share and Delete. Audio plays in a panel with the full-size player.

## Diagnostics

Set `ARTCRAFT_SCREENSHOT=<file.png>` to save a screenshot once the window has settled.
`ARTCRAFT_PAGE` (`home`, `create-image`, `create-video`, `create-audio` or `library`) picks the page,
`ARTCRAFT_OPEN` (`settings` or `midjourney-login`) opens a dialog, and `ARTCRAFT_SCREENSHOT_EXIT=1`
closes the app afterwards. The screenshot shows egui only, not the sign-in webview.

## Tests

```bash
cargo test -p artcraft_native
```

There are unit tests for the catalog, feed store, wire decoding and backend helpers. Headless UI
tests (`egui_kittest`) drive the prompt box (generate gating, Enter to generate, the pickers, `@`
mentions, clear-all), the Characters dialog and the audio toolbar (toggles, Beat & Key, Tuning).

## Fonts and brand assets

`assets/fonts` holds Inter, Archivo and Geist Mono, all under the SIL Open Font License; the
licences sit next to the files. The wordmark, app icon and creator logos are the Tauri app's own
(`frontend/apps/artcraft/app/public/resources`, `crates/desktop/artcraft/icons`); see `NOTICE`
for their terms.
