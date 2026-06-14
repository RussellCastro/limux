# Browser import

`limux browser profiles` discovers local Chrome-family and Firefox profile
stores. `limux browser profile-data` creates a consent-gated manifest or staged
copy of raw browser-owned cookie, history, and session stores. `limux browser
import-cookies` imports cookie files into the currently selected WebKit browser
surface. Current hosts use WebKitGTK's `CookieManager`, so `httpOnly`, domain,
path, secure, and expiration/max-age attributes are preserved for imported rows
when the export includes them. This is still not a full Chrome, Firefox, or Arc
profile importer.

## Usage

```bash
limux browser profiles
limux browser profiles --browser firefox --include-missing
limux browser profile-data --profile-path ~/.config/google-chrome/Default --dry-run
limux browser profile-data --profile-path ~/.mozilla/firefox/abc.default-release --types sessions --dry-run
limux browser profile-data --profile-path ~/.config/google-chrome/Default --allow-profile-read --out-dir ./limux-profile-stage
limux browser --surface "$LIMUX_SURFACE_ID" import-cookies --file ./cookies.json
limux browser "$LIMUX_SURFACE_ID" import-cookies ./cookies.txt --format netscape
```

`profiles` returns JSON rows for discovered local browser profiles, including
candidate cookie, history, and session-store paths. It currently covers common
Linux Chrome-family roots under `XDG_CONFIG_HOME`/`~/.config` plus Firefox
profiles under `~/.mozilla/firefox`. Use `--browser <name>` to filter by browser
id/name and `--include-missing` to report known browser roots that were not
found.

## Profile-data staging

`browser profile-data` is the explicit consent boundary for reading raw browser
profile stores. With `--dry-run`, it returns a JSON manifest of candidate stores
without copying profile data. To copy stores into a Limux-controlled staging
directory, pass both `--allow-profile-read` and `--out-dir <dir>`.

Supported families are `chromium` and `firefox`; when `--family` is omitted,
Limux infers the family from known profile files. `--types` accepts a
comma-separated subset of `cookies`, `history`, and `sessions`.

The staging command copies regular files and directories without overwriting
existing destination files, and skips symlinks. For Chromium profiles it knows
`Network/Cookies`, legacy `Cookies`, `History`, and `Sessions`. For Firefox it
knows `cookies.sqlite`, `places.sqlite`, `sessionstore.jsonlz4`, and
`sessionstore-backups`.

The staged files are raw browser-owned stores. Limux does not decrypt cookies,
parse browser history databases, or import browser sessions into WebKit from
this command yet.

## Cookie-file import

`--format auto` is the default for `browser import-cookies`. `.json` files are
parsed as JSON; other file extensions default to Netscape `cookies.txt` format.
Use `--format json` or `--format netscape` to force a parser.

The cookie import command returns JSON with `imported_count`, `skipped_count`,
`imported`, and `skipped` rows. When connected to an older host that does not expose
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

Profile discovery reports local store paths only, and profile-data staging
copies raw browser-owned stores only after explicit consent. Reading SQLite rows,
decrypting browser-owned cookie values, importing browser history into WebKit,
importing browser sessions, and Arc-specific profile discovery remain open
cmux-parity work.
