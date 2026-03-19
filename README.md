# solcast-proxy

Caching reverse proxy for the [Solcast](https://solcast.com/) solar forecast API.

Solcast's free tier allows 10 API calls/day. If you run multiple clients (e.g. two solar monitoring apps), each one burns through the quota independently. This proxy sits in between and caches responses so only one set of upstream calls is made, regardless of how many clients hit it.

## Install

```bash
brew install bradleydwyer/tap/solcast-proxy
```

Or from source:

```bash
cargo build --release
./target/release/solcast-proxy
```

## Usage

Point your Solcast clients at the proxy instead of `api.solcast.com.au`:

```bash
solcast-proxy                     # starts on port 8888

curl -H "Authorization: Bearer YOUR_KEY" \
  http://localhost:8888/rooftop_sites/YOUR_SITE_ID/forecasts
```

### Options

```
-p, --port <PORT>         Listen port [default: 8888]
-c, --cache-dir <DIR>     Cache directory [default: ./data]
--ttl <SECS>              Cache TTL in seconds [default: 7200]
--rate-limit <SECS>       Min seconds between upstream calls per endpoint [default: 9000]
```

## How it works

The proxy forwards requests upstream, caches the response body, and serves it back on later requests. Auth is pass-through: clients send their own Bearer token and the proxy forwards it.

Responses include `X-Cache: HIT|MISS|STALE` and `X-Cache-Age` headers so you can tell what happened.

Cache is persisted to disk and survives restarts.

Send `Cache-Control: no-cache` to force a fresh upstream fetch. This bypasses the TTL and rate limit.

## Deploying as a service

A systemd unit file is included. `deploy.sh` builds, installs the binary to `/usr/local/bin`, and enables the service.

## License

MIT

## More Tools

**Naming & Availability**
- [available](https://github.com/bradleydwyer/available) — AI-powered project name finder (uses parked, staked & published)
- [parked](https://github.com/bradleydwyer/parked) — Domain availability checker (DNS → WHOIS → RDAP)
- [staked](https://github.com/bradleydwyer/staked) — Package registry name checker (npm, PyPI, crates.io + 19 more)
- [published](https://github.com/bradleydwyer/published) — App store name checker (App Store & Google Play)

**AI Tooling**
- [sloppy](https://github.com/bradleydwyer/sloppy) — AI prose/slop detector
- [caucus](https://github.com/bradleydwyer/caucus) — Multi-LLM consensus engine
- [nanaban](https://github.com/bradleydwyer/nanaban) — Gemini image generation CLI
- [equip](https://github.com/bradleydwyer/equip) — Cross-agent skill manager
