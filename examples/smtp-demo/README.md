# SMTP Email Component

A WebAssembly component that sends emails via SMTP with support for attachments downloaded from URLs, with non-tls & auto-tls support

## Features

-  **URL-based Attachments**: Downloads files from URLs and attaches them to emails
-  **Multiple Recipients**: Supports To, CC, and BCC fields
-  **Flexible Configuration**: Configure SMTP server, credentials, and email content via JSON

## Prerequisites

- `cargo` 1.82+
- [`wash`](https://wasmcloud.com/docs/installation) 0.36.1+
- `wasmtime` >=25.0.0 (if running with wasmtime)

## Building

```bash
wash build
```

## Running with wasmtime

You must have wasmtime >=25.0.0 for this to work. Make sure to follow the build step above first.

```bash
wasmtime serve -Scommon ./build/smtp_component_s.wasm
```

## Running with wasmCloud

```bash
wash dev
```

## Usage

Send a POST request with JSON configuration:

### Basic Email

```bash
curl -X POST http://127.0.0.1:8000 \
  -H "Content-Type: application/json" \
  -d '{
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 465,
      "username": "your-email@gmail.com",
      "password": "your-app-password"
    },
    "from": "your-email@gmail.com",
    "to": "recipient@example.com",
    "subject": "Hello from SMTP Component",
    "body": "This is a test email sent from a WebAssembly component!"
  }'
```

### Email with Multiple Recipients

```bash
curl -X POST http://127.0.0.1:8000 \
  -H "Content-Type: application/json" \
  -d '{
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 587,
      "username": "your-email@gmail.com",
      "password": "your-app-password"
    },
    "from": "your-email@gmail.com",
    "to": ["recipient1@example.com", "recipient2@example.com"],
    "cc": ["cc@example.com"],
    "bcc": ["bcc@example.com"],
    "subject": "Team Update",
    "body": "This email goes to multiple recipients."
  }'
```

### Email with Attachments from URLs

```bash
curl -X POST http://127.0.0.1:8000 \
  -H "Content-Type: application/json" \
  -d '{
    "smtp": {
      "host": "smtp.gmail.com",
      "port": 465,
      "username": "your-email@gmail.com",
      "password": "your-app-password"
    },
    "from": "your-email@gmail.com",
    "to": "recipient@example.com",
    "subject": "Report with Attachments",
    "body": "Please find the attached documents.",
    "attachments": [
      {
        "url": "https://example.com/report.pdf",
        "filename": "monthly-report.pdf",
        "content_type": "application/pdf"
      },
      {
        "url": "https://example.com/data.csv",
        "filename": "data.csv",
        "content_type": "text/csv"
      }
    ]
  }'
```

## Configuration Reference

### SMTP Settings

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `smtp.host` | string | No | `smtp.gmail.com` | SMTP server hostname |
| `smtp.port` | number | No | `465` | SMTP server port (465 for SSL, 587 for STARTTLS) |
| `smtp.username` | string | Yes | - | SMTP authentication username |
| `smtp.password` | string | Yes | - | SMTP authentication password |

### Email Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `from` | string | Yes | - | Sender email address |
| `to` | string or array | Yes | - | Recipient email address(es) |
| `cc` | string or array | No | - | CC recipient(s) |
| `bcc` | string or array | No | - | BCC recipient(s) |
| `subject` | string | No | `Test Email from SMTP Component` | Email subject line |
| `body` | string | No | `Test email from SMTP component` | Email body (plain text) |

### Attachment Fields

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `attachments[].url` | string | Yes | - | URL to download the attachment from |
| `attachments[].filename` | string | No | Extracted from URL | Name for the attachment |
| `attachments[].content_type` | string | No | `application/octet-stream` | MIME type of the attachment |

## TLS Configuration

The component automatically selects the appropriate TLS mode based on the port:

- **Port 465**: Implicit TLS (SSL) - Direct encrypted connection
- **Ports 587, 25, 2525**: Explicit TLS (STARTTLS) - Upgrade plain connection to encrypted

## Gmail Configuration

For Gmail accounts, you'll need to:

1. Enable 2-factor authentication
2. Generate an [App Password](https://myaccount.google.com/apppasswords)
3. Use the app password instead of your regular password

## Error Handling

The component provides detailed logging and error messages:

-  Success messages include server response and message ID
-  Errors include context about what failed (connection, authentication, sending)
-  Logs track each step: connection, attachment download, email sending

## Adding Capabilities

To learn how to extend this example with additional capabilities, see the [Adding Capabilities](https://wasmcloud.com/docs/tour/adding-capabilities?lang=rust) section of the wasmCloud documentation.