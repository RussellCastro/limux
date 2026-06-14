use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const PROJECT_COMMAND_CONFIG_FILES: [&str; 2] = ["cmux.json", "limux.json"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCommandDefinition {
    pub name: String,
    pub label: String,
    pub command: String,
    pub cwd: Option<String>,
    pub source: PathBuf,
}

fn nonempty_trimmed(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn json_string_for_keys(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match map.get(*key) {
        Some(Value::String(value)) => nonempty_trimmed(value),
        Some(Value::Number(value)) => Some(value.to_string()),
        _ => None,
    })
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn command_string_from_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => nonempty_trimmed(value),
        Value::Array(items) => {
            let mut args = Vec::new();
            for item in items {
                let Value::String(value) = item else {
                    return None;
                };
                args.push(shell_single_quote(value));
            }
            if args.is_empty() {
                None
            } else {
                Some(args.join(" "))
            }
        }
        Value::Object(map) => ["command", "cmd", "run", "script", "shell"]
            .iter()
            .find_map(|key| map.get(*key).and_then(command_string_from_value)),
        _ => None,
    }
}

fn parse_project_command_entry(
    name_hint: Option<&str>,
    value: &Value,
    source: &Path,
) -> Result<ProjectCommandDefinition, String> {
    if let Value::String(command) = value {
        let name = name_hint
            .and_then(nonempty_trimmed)
            .ok_or_else(|| "string project commands require a map key name".to_string())?;
        let command = nonempty_trimmed(command)
            .ok_or_else(|| format!("project command `{name}` has an empty command"))?;
        return Ok(ProjectCommandDefinition {
            label: name.clone(),
            name,
            command,
            cwd: None,
            source: source.to_path_buf(),
        });
    }

    let Value::Object(map) = value else {
        return Err("project command entries must be strings or objects".to_string());
    };

    let command = command_string_from_value(value).ok_or_else(|| {
        format!(
            "project command `{}` is missing command/cmd/run/script/shell",
            name_hint.unwrap_or("<unnamed>")
        )
    })?;
    let name = json_string_for_keys(map, &["id", "name", "key"])
        .or_else(|| name_hint.and_then(nonempty_trimmed))
        .or_else(|| json_string_for_keys(map, &["label", "title"]))
        .ok_or_else(|| "project command object is missing a name or id".to_string())?;
    let label = json_string_for_keys(map, &["label", "title", "name"])
        .unwrap_or_else(|| name.clone());
    let cwd = json_string_for_keys(map, &["cwd", "working_directory", "workingDir"]);

    Ok(ProjectCommandDefinition {
        name,
        label,
        command,
        cwd,
        source: source.to_path_buf(),
    })
}

pub fn parse_project_commands_from_value(
    value: &Value,
    source: &Path,
) -> Result<Vec<ProjectCommandDefinition>, String> {
    let root = value
        .as_object()
        .ok_or_else(|| format!("{} must contain a JSON object", source.display()))?;
    let commands = root
        .get("commands")
        .or_else(|| root.get("customCommands"))
        .ok_or_else(|| {
            format!(
                "{} does not contain a commands object or array",
                source.display()
            )
        })?;

    let mut parsed = Vec::new();
    match commands {
        Value::Object(map) => {
            for (name, value) in map {
                parsed.push(parse_project_command_entry(Some(name), value, source)?);
            }
        }
        Value::Array(items) => {
            for (index, value) in items.iter().enumerate() {
                parsed.push(parse_project_command_entry(None, value, source).map_err(|err| {
                    format!("invalid project command at commands[{index}]: {err}")
                })?);
            }
        }
        _ => return Err(format!("{} commands must be an object or array", source.display())),
    }
    Ok(parsed)
}

pub fn load_project_commands_from_path(
    path: &Path,
) -> Result<Vec<ProjectCommandDefinition>, String> {
    let raw = fs::read_to_string(path).map_err(|err| {
        format!("failed to read project command config {}: {err}", path.display())
    })?;
    let value: Value = serde_json::from_str(&raw).map_err(|err| {
        format!(
            "project command config {} is not valid JSON: {err}",
            path.display()
        )
    })?;
    parse_project_commands_from_value(&value, path)
}

pub fn find_project_command_config_in(start: &Path) -> Option<PathBuf> {
    let mut current = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };

    loop {
        for file_name in PROJECT_COMMAND_CONFIG_FILES {
            let candidate = current.join(file_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn resolved_project_command_cwd(
    definition: &ProjectCommandDefinition,
    override_cwd: Option<&str>,
) -> String {
    let raw = override_cwd
        .and_then(nonempty_trimmed)
        .or_else(|| definition.cwd.clone())
        .unwrap_or_else(|| {
            definition
                .source
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_string_lossy()
                .to_string()
        });
    let path = PathBuf::from(&raw);
    if path.is_absolute() {
        raw
    } else {
        definition
            .source
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(path)
            .to_string_lossy()
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn parses_object_and_array_project_commands() {
        let source = Path::new("/repo/cmux.json");
        let parsed = parse_project_commands_from_value(
            &json!({
                "commands": {
                    "dev": "npm run dev",
                    "test": { "label": "Tests", "command": "npm test", "cwd": "web" }
                }
            }),
            source,
        )
        .unwrap();

        assert_eq!(parsed[0].name, "dev");
        assert_eq!(parsed[0].label, "dev");
        assert_eq!(parsed[0].command, "npm run dev");
        assert_eq!(parsed[1].name, "test");
        assert_eq!(parsed[1].label, "Tests");
        assert_eq!(parsed[1].cwd.as_deref(), Some("web"));

        let array = parse_project_commands_from_value(
            &json!({
                "customCommands": [
                    { "id": "lint", "run": ["cargo", "clippy"], "cwd": "." }
                ]
            }),
            source,
        )
        .unwrap();
        assert_eq!(array[0].name, "lint");
        assert_eq!(array[0].command, "'cargo' 'clippy'");
    }

    #[test]
    fn finds_nearest_project_command_config_with_cmux_priority() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join("cmux.json"), r#"{"commands":{"root":"echo root"}}"#).unwrap();
        std::fs::write(
            nested.join("limux.json"),
            r#"{"commands":{"nested":"echo nested"}}"#,
        )
        .unwrap();
        assert_eq!(
            find_project_command_config_in(&nested),
            Some(nested.join("limux.json"))
        );

        std::fs::write(
            nested.join("cmux.json"),
            r#"{"commands":{"cmux":"echo cmux"}}"#,
        )
        .unwrap();
        assert_eq!(
            find_project_command_config_in(&nested),
            Some(nested.join("cmux.json"))
        );
    }

    #[test]
    fn resolves_relative_cwd_from_config_directory() {
        let definition = ProjectCommandDefinition {
            name: "test".to_string(),
            label: "Tests".to_string(),
            command: "npm test".to_string(),
            cwd: Some("web".to_string()),
            source: PathBuf::from("/repo/cmux.json"),
        };
        assert_eq!(resolved_project_command_cwd(&definition, None), "/repo/web");
        assert_eq!(
            resolved_project_command_cwd(&definition, Some("/tmp/scratch")),
            "/tmp/scratch"
        );
    }
}
