# Ghostty compatibility

Limux embeds Ghostty for terminal rendering through `libghostty.so`. The embedded
runtime is initialized with Ghostty's default config loader and recursive config
loader, so terminal-renderer settings continue to come from the normal Ghostty
configuration files, typically `~/.config/ghostty/config` or the platform's
XDG config path.

## What Ghostty owns

Ghostty's own config parser handles renderer and terminal behavior keys. Limux
passes the loaded config into `ghostty_app_new`, `ghostty_app_update_config`, and
`ghostty_surface_update_config` rather than reimplementing those settings.

This covers Ghostty-owned keys such as:

- font family, font style, font features, and cell metrics
- themes, foreground/background colors, palette entries, cursor colors, and
  selection colors
- shell integration and terminal behavior that Ghostty implements internally
- recursive Ghostty config includes

## What Limux mirrors

A few Ghostty settings affect GTK chrome around the embedded surface, so Limux
also reads or mirrors them explicitly:

| Key or setting | Limux behavior |
|---|---|
| `font-size` | Seeds Limux's terminal default/reset size before a surface is created. Values are clamped to `1..=255`; invalid or missing values default to `12`. |
| `background-opacity` | Read through the loaded Ghostty config and mirrored into the Limux content/window background so translucent terminal backgrounds do not sit on an opaque host panel. Values are clamped to `0..=1`. |
| `scrollbar = never` | Hides Limux's GTK scrollbar wrapper. Other values and missing values allow the scrollbar to appear when Ghostty reports scrollback larger than the viewport. |
| Ghostty reload action | Reloads Ghostty config and updates app/surface config; Limux also refreshes the mirrored scrollbar flag. |

Limux also has its own `~/.config/limux/config.json` appearance setting named
`ghostty_color_scheme`. That is not a Ghostty config key; it controls whether
Limux asks Ghostty surfaces to use dark, light, or system color-scheme mode.

## Boundaries

Limux does not provide a Ghostty config editor, does not validate every Ghostty
key, and does not translate Ghostty config into Limux-specific config. If a key
belongs to Ghostty's renderer or terminal model, Ghostty remains the source of
truth.
