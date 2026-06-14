# Browser import

`limux browser import-cookies` imports cookie files into the currently selected
WebKit browser surface. It is a first cmux-parity slice for cookie-file import,
not a full Chrome, Firefox, or Arc profile importer.

## Usage

```bash
limux browser --surface "$LIMUX_SURFACE_ID" import-cookies --file ./cookies.json
limux browser "$LIMUX_SURFACE_ID" import-cookies ./cookies.txt --format netscape
```

`--format auto` is the default. `.json` files are parsed as JSON; other file
extensions default to Netscape `cookies.txt` format. Use `--format json` or
`--format netscape` to force a parser.

The command returns JSON with `imported_count`, `skipped_count`, `imported`, and
`skipped` rows. `httpOnly` cookies are reported as skipped because the current
bridge sets cookies through the active page's `document.cookie` path.

## JSON format

JSON can be either an array of cookie objects or an object with a `cookies`
array. Accepted keys are:

- `name` or `key`
- `value`
- `domain` or `host`
- `path`
- `secure`
- `httpOnly` or `http_only`

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

Rows prefixed with `#HttpOnly_` are parsed, but skipped during import for the
same page-bridge limitation.

## Limits

The page bridge can only set cookies for the page currently loaded in the
WebKit surface. Domain and path values are preserved in the report but are not
enforced by the current `browser.cookies.set` bridge. Import the file after
navigating the browser surface to the target origin.

Full browser profile discovery, native WebKitGTK cookie-store import, browser
history import, and session import remain open cmux-parity work.
