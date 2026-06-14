# Browser import

`limux browser profiles` discovers local Chrome-family and Firefox profile
stores. `limux browser import-cookies` imports cookie files into the currently
selected WebKit browser surface. Current hosts use WebKitGTK's `CookieManager`,
so `httpOnly`, domain, path, secure, and expiration/max-age attributes are
preserved for imported rows when the export includes them.
It is still a discovery plus cookie-file import slice, not a full Chrome,
Firefox, or Arc profile importer.

## Usage

```bash
limux browser profiles
limux browser profiles --browser firefox --include-missing
limux browser --surface "$LIMUX_SURFACE_ID" import-cookies --file ./cookies.json
limux browser "$LIMUX_SURFACE_ID" import-cookies ./cookies.txt --format netscape
```

`profiles` returns JSON rows for discovered local browser profiles, including
candidate cookie, history, and session-store paths. It currently covers common
Linux Chrome-family roots under `XDG_CONFIG_HOME`/`~/.config` plus Firefox
profiles under `~/.mozilla/firefox`. Use `--browser <name>` to filter by browser
id/name and `--include-missing` to report known browser roots that were not
found.

`--format auto` is the default. `.json` files are parsed as JSON; other file
extensions default to Netscape `cookies.txt` format. Use `--format json` or
`--format netscape` to force a parser.

The command returns JSON with `imported_count`, `skipped_count`, `imported`, and
`skipped` rows. When connected to an older host that does not expose
`browser.cookies.import`, the CLI falls back to the legacy page bridge; that
fallback can only set page-visible cookies and reports `httpOnly` rows as
skipped.

## JSON format

JSON can be either an array of cookie objects or an object with a `cookies`
array. Accepted keys are:

- `name` or `key`
- `value`
- `domain` or `host`
- `path`
- `secure`
- `httpOnly` or `http_only`
- `expirationDate`, `expires`, `expiry`, `expiration`, `expires_unix`,
  `maxAge`, or `max_age`

Example:

```json
{
  "cookies": [
    {
      "name": "sid",
      "value": "abc123",
      "domain": ".example.com",
      "path": "/",
      "secure": true
    }
  ]
}
```

## Netscape format

Netscape `cookies.txt` rows use the standard tab-separated columns:

```text
.example.com	TRUE	/	TRUE	0	sid	abc123
#HttpOnly_.example.com	TRUE	/	FALSE	0	private	secret
```

Rows prefixed with `#HttpOnly_` are parsed and imported through WebKitGTK's
cookie manager on current hosts. They are skipped only when the CLI is connected
to an older host that lacks `browser.cookies.import`.

## Limits

Rows with no domain use the currently loaded page host. If the active browser
surface is on `about:blank` or another hostless URI, domainless rows are
reported as skipped. Import the file after navigating the browser surface to the
target origin when your export omits domains.

Profile discovery reports local store paths only. Reading Chrome-family SQLite
cookie stores, decrypting browser-owned cookie values, importing browser
history, importing sessions, and Arc-specific profile discovery remain open
cmux-parity work.
