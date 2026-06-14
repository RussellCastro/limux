# Project Custom Commands

Limux can read project-scoped command definitions from `cmux.json` or
`limux.json`. This is the first custom-command parity slice: commands can be
listed and launched from the CLI, while in-app command palette integration is
still tracked in the cmux parity contract.

## Discovery

`limux commands` searches upward from the current directory for:

1. `cmux.json`
2. `limux.json`

Use `--project <path>` to choose a different search root, or `--config <path>`
to point at an exact file.

## Schema

The root object must contain `commands` or `customCommands`. Object and array
forms are accepted.

```json
{
  "commands": {
    "dev": "npm run dev",
    "test": {
      "label": "Tests",
      "command": "npm test",
      "cwd": "web"
    }
  }
}
```

```json
{
  "commands": [
    {
      "id": "lint",
      "label": "Lint",
      "run": "cargo clippy",
      "cwd": "."
    }
  ]
}
```

Supported command body keys are `command`, `cmd`, `run`, `script`, and `shell`.
A command body can be a string or an argv-style string array. Relative `cwd`
values resolve relative to the config file directory; commands without `cwd`
run from the config file directory.

## CLI

```bash
limux commands
limux commands --project ~/src/my-app
limux commands --config ./cmux.json --json

limux run-command dev
limux commands run test --name "Project tests"
limux run-command --config ./limux.json --cwd /tmp/scratch lint
```

Running a command creates a new workspace through the live `workspace.create`
bridge, sets the workspace title from the command label unless `--name` is
provided, sets `cwd`, and launches the configured command in the first terminal.
