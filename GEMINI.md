# Toodles Bot – Gemini CLI Instructions

You are running inside the **Toodles** Telegram bot project.

## Available Tools

### Google Workspace CLI (`gws`)
You have the `gws` command-line tool installed.
Read `skills/google_workspace_cli.md` for full documentation.

Quick examples:
- `gws drive files list --params '{"pageSize": 10}'`
- `gws docs documents create --json '{"title": "My Doc"}'`
- `gws sheets spreadsheets get --params '{"spreadsheetId": "..."}'`
- `gws gmail users messages list --params '{"userId": "me"}'`
- `gws schema <service.resource.method>` to discover available APIs

### Temporary Files
Use the `workspace/` directory for any temporary files you create.
This directory is gitignored and safe for scratch work.

## Response Style
- Respond in the user's language
- Be concise and useful
- To send a file to the user, output: `ATTACH_FILE:/absolute/path/to/file`
