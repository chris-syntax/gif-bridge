# gif-bridge

Giphy search/cache proxy for [loaf-chat](https://github.com/chris-syntax/loaf-chat)'s GIF picker (`net.loaf.gif` message type).

Two endpoints:

- `GET /search?q=&limit=&openid_token=` — proxies Giphy search, injecting the API key server-side.
- `GET /media/:id?openid_token=` — lazily fetches and in-memory caches an individual gif's bytes.

Both endpoints require a live Matrix OpenID token on every request, verified against the homeserver's federation `openid/userinfo` endpoint each time — no static or permanent bearer capability. No media is ever uploaded into a homeserver's own media store; the cache is purely in-memory and TTL/size-bounded.

## Config

- `GIPHY_API_KEY` (required)
- `OPENID_VERIFY_URL` (default `https://matrix.loaf.moe/_matrix/federation/v1/openid/userinfo`)
- `PORT` (default `8080`)

## Deployment

Deployed via [chris-syntax/gitops](https://github.com/chris-syntax/gitops)'s `matrix` branch, which pulls the image this repo's CI publishes to `ghcr.io/chris-syntax/gif-bridge`.
