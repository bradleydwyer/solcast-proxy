# solcast-proxy

Caching reverse proxy for the [Solcast](https://solcast.com/) solar forecast API.

Solcast's free tier allows 10 API calls/day. If you're running multiple clients (e.g. two instances of a solar monitoring app), they each burn through the quota independently. This proxy sits in between and serves cached responses so only one set of upstream calls is made regardless of how many clients are hitting it.

## How it works

Point your Solcast clients at the proxy instead of `api.solcast.com.au`. The proxy forwards requests upstream, caches the full response body, and serves it back on subsequent requests. Auth is pass-through — clients send their own Bearer token and the proxy forwards it.

Responses include `X-Cache: HIT|MISS|STALE` and `X-Cache-Age` headers so you can see what's happening.

Cache is persisted to disk and survives restarts.

## Usage

```
cargo build --release
./target/release/solcast-proxy
```

Options:

```
-p, --port <PORT>         Listen port [default: 8888]
-c, --cache-dir <DIR>     Cache directory [default: ./data]
--ttl <SECS>              Cache TTL in seconds [default: 7200]
--rate-limit <SECS>       Min seconds between upstream calls per endpoint [default: 9000]
```

Then point your client at `http://localhost:8888` instead of `https://api.solcast.com.au`:

```
curl -H "Authorization: Bearer YOUR_KEY" \
  http://localhost:8888/rooftop_sites/YOUR_SITE_ID/forecasts
```

## Deploying as a service

A systemd unit file is included. `deploy.sh` builds, installs the binary to `/usr/local/bin`, and enables the service.

## License

MIT
