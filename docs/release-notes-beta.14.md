# v0.1.0-beta.14

This beta fixes background stripes left behind when closing or moving completion
overlays containing Chinese and other wide characters.

- Overlay cleanup now explicitly clears every column occupied by a wide glyph,
  including its continuation cells, while preserving replacement shell text.
- Directory errors appear in a compact notice with the cause and recovery
  guidance before the path. Long messages wrap within the notice, and normal
  completion returns when the path is corrected.
- HTTP clients honor system proxy settings and support SOCKS proxies alongside
  the standard proxy environment variables.
- Update checks explain GitHub API rate limits and the retry time instead of
  reporting a generic HTTP failure.

Real-terminal certification remains subject to the
[compatibility matrix](https://github.com/backrunner/hokan/blob/v0.1.0-beta.14/docs/compatibility.md);
this beta does not expand those certification claims.

Existing beta users can update with:

```sh
hokan upgrade --channel beta --yes
```

Restart Hokan after upgrading to use the new binary.
