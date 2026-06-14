const DEFAULT_FONT_SIZE: f32 = 12.0;
const DEFAULT_BACKGROUND_OPACITY: f64 = 1.0;

fn ghostty_config_contents() -> Option<String> {
    let path = dirs::config_dir()
        .map(|d| d.join("ghostty/config"))
        .filter(|p| p.exists())?;
    std::fs::read_to_string(&path).ok()
}

fn read_ghostty_value(contents: &str, key: &str) -> Option<String> {
    let mut found = None;
    for line in contents.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        if raw_key.trim() == key {
            found = Some(raw_value.trim().to_string());
        }
    }
    found
}

fn numeric_prefix(value: &str) -> &str {
    value.split('#').next().unwrap_or(value).trim()
}

fn read_background_opacity_from_contents(contents: &str) -> f64 {
    read_ghostty_value(contents, "background-opacity")
        .and_then(|value| numeric_prefix(&value).parse::<f64>().ok())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(DEFAULT_BACKGROUND_OPACITY)
}

fn read_font_size_from_contents(contents: &str) -> f32 {
    read_ghostty_value(contents, "font-size")
        .and_then(|value| numeric_prefix(&value).parse::<f32>().ok())
        .map(|value| value.clamp(1.0, 255.0))
        .unwrap_or(DEFAULT_FONT_SIZE)
}

/// Read background-opacity from the Ghostty config file.
/// Returns a value between 0.0 and 1.0 (default: 1.0 = fully opaque).
#[allow(dead_code)]
pub fn read_background_opacity() -> f64 {
    ghostty_config_contents()
        .as_deref()
        .map(read_background_opacity_from_contents)
        .unwrap_or(DEFAULT_BACKGROUND_OPACITY)
}

/// Read font-size from the Ghostty config file.
/// Returns the configured size in points (default: 12.0).
pub fn read_font_size() -> f32 {
    ghostty_config_contents()
        .as_deref()
        .map(read_font_size_from_contents)
        .unwrap_or(DEFAULT_FONT_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ghostty_value_parser_matches_exact_keys_and_last_assignment() {
        let contents = r#"
            # font-size = 99
            font-size-extra = 40
            font-size = 13
            font-size = 14 # later assignment wins
            background-opacity = 0.7
        "#;

        assert_eq!(
            read_ghostty_value(contents, "font-size").as_deref(),
            Some("14 # later assignment wins")
        );
        assert_eq!(read_font_size_from_contents(contents), 14.0);
        assert_eq!(read_background_opacity_from_contents(contents), 0.7);
    }

    #[test]
    fn ghostty_numeric_values_are_clamped_and_defaulted() {
        assert_eq!(
            read_font_size_from_contents("font-size = nope"),
            DEFAULT_FONT_SIZE
        );
        assert_eq!(read_font_size_from_contents("font-size = -20"), 1.0);
        assert_eq!(read_font_size_from_contents("font-size = 999"), 255.0);
        assert_eq!(
            read_background_opacity_from_contents("background-opacity = nope"),
            DEFAULT_BACKGROUND_OPACITY
        );
        assert_eq!(
            read_background_opacity_from_contents("background-opacity = -1"),
            0.0
        );
        assert_eq!(
            read_background_opacity_from_contents("background-opacity = 2"),
            1.0
        );
    }
}
