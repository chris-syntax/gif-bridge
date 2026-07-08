# gif-bridge

Giphy search/cache proxy for [loaf-chat](https://github.com/chris-syntax/loaf-chat)'s GIF picker (`moe.loaf.gif` message type).

## Endpoints

- `GET /search?q=&limit=` — proxies Giphy search, injecting the API key server-side. Requires a Matrix OpenID token as `Authorization: Bearer <token>`, verified against the homeserver's federation `openid/userinfo` endpoint (verified tokens are cached for 15 minutes). Responses are cached in memory for 1 hour per normalized `(q, limit)`. Each result carries a `thumbUrl` (Giphy `fixed_width` rendition, for the picker grid) and a `fullUrl` (`original` rendition, for the sent message).
- `GET /media/:id/:variant` — serves a gif's bytes, where `:variant` is `thumb` or `full`. Unauthenticated: ids are only learned through an authenticated search, and this path **never calls the Giphy API** — the CDN urls are recorded at search time, so an unknown id is a 404, not an API call. Cold fetches stream from Giphy's CDN to the client while being written to the disk cache; responses are `Cache-Control: public, max-age=31536000, immutable`.

The only `api.giphy.com` traffic is one call per uncached search; gif bytes come from `media.giphy.com`, which doesn't count against API quota.

## Cache

Media bytes are cached on disk (`$CACHE_DIR/media/`, LRU-evicted by total bytes) and survive restarts — mount a persistent volume at `CACHE_DIR` in deployment. Alongside them, tiny `id → CDN url` records (`$CACHE_DIR/urls/`) let gifs in old messages be re-fetched after eviction without any API lookup. Downloads commit atomically (temp file + rename), so a crash never leaves a truncated entry. Concurrent requests for the same rendition are coalesced into one CDN download.

## Config

- `GIPHY_API_KEY` (required)
- `OPENID_VERIFY_URL` (default `https://matrix.loaf.moe/_matrix/federation/v1/openid/userinfo`)
- `PORT` (default `8080`)
- `CACHE_DIR` (default `./cache`)
- `CACHE_MAX_BYTES` (default `2147483648`, i.e. 2 GiB — bounds media bytes on disk)

## Deployment

Deployed via [chris-syntax/gitops](https://github.com/chris-syntax/gitops)'s `matrix` branch, which builds this repo directly from git at deploy time (same pattern as the `invites` branch) — no registry image, no CI.
