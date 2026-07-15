# Quarto preview and render

Raven provides a standalone, CLI-backed Quarto workflow for `.qmd` files. It
uses the public Quarto CLI rather than the Quarto VS Code extension:

- **`Raven: Quarto Preview`** starts Quarto's live preview server and embeds
  the served page in a VS Code panel.
- **`Raven: Quarto Render`** runs a one-shot render and reports the output
  file.
- **`Raven: Stop Quarto Preview`** stops the preview associated with the
  current document.
- **`Raven: Show Quarto Output`** opens the shared **Raven: Quarto** output
  channel.

Quarto commands are available from the Command Palette and, for `.qmd` files,
inside the editor-title **Send to R** ($(play)) menu. A dedicated $(preview)
button in the editor title bar starts Quarto Preview directly. Press
`Shift+Cmd+Enter` on macOS or `Shift+Ctrl+Enter` on Windows/Linux for the same
action. For `.Rmd` / `.Rmarkdown` files, the same button and shortcut run Knit
Preview, providing one consistent preview action across both formats. You can
right-click the editor toolbar and choose **Hide** if you do not want the
button.

The direct Quarto Preview button and palette commands do not depend on Raven's
R console or on the `quarto.quarto` extension. The **Send to R** menu itself is
shown only when Raven's R console is enabled, so Render and Stop remain
palette-accessible when that console is off. Preview and Render execute the
document through Quarto and therefore require a trusted workspace. Stop remains
available in Restricted Mode so you can terminate an existing preview.

Raven's [Knit Preview](knit.md) remains a separate pipeline for `.Rmd` and
`.Rmarkdown` files. Knit Preview does not handle `.qmd`, and Quarto Preview does
not use Raven's knit renderer.

## Requirements and Quarto discovery

Install the [Quarto CLI](https://quarto.org/docs/get-started/) on the machine
where the VS Code extension host runs. For a local window, that is your local
machine. Under Remote SSH, it is the remote machine.

Raven resolves Quarto lazily for the active document, in this order:

1. The effective `raven.quarto.path` setting, when non-empty. Set this to the
   absolute path of the Quarto executable. If the configured path is unusable
   or is not Quarto, Raven reports it instead of silently trying another
   executable.
2. `quarto` on the extension host's `PATH`.
3. Standard platform locations:
   - macOS: `/opt/homebrew/bin/quarto`, `/usr/local/bin/quarto`, then RStudio's
     bundled Quarto at
     `/Applications/RStudio.app/Contents/Resources/app/quarto/bin/quarto`.
   - Windows: `%LOCALAPPDATA%\Programs\Quarto\bin\quarto.exe`, then
     `%PROGRAMFILES%\Quarto\bin\quarto.exe` when those environment variables
     are defined.
   - Linux: `/usr/local/bin/quarto`, then `/opt/quarto/bin/quarto`.

Each candidate must identify itself as Quarto through `quarto --help`. In a
multi-root workspace, `raven.quarto.path` is resolved for the source document,
so folders can use different effective paths.

If Raven cannot find Quarto, the error offers **Install…** and **Set Path…**.

## Commands

### Quarto Preview

`Raven: Quarto Preview` saves a dirty document first. If saving is refused or
fails, Raven stops because Quarto would otherwise render stale content. Raven
also requires a saved `.qmd` file opened from the filesystem. Virtual documents
such as Git-diff revisions are rejected so Quarto cannot render a different
on-disk revision from the one shown in the editor. Raven then runs the
equivalent of:

```text
quarto preview <file> --no-browser --host 127.0.0.1
```

The executable and arguments are passed directly to the process; Raven does
not invoke a shell. The working directory is the nearest Quarto project root,
as described under [Preview ownership and keying](#preview-ownership-and-keying),
or the document's directory when there is no project marker.

Quarto prints its serving URL before the server is necessarily ready. Raven
therefore:

1. reads both stdout and stderr and strips terminal control sequences;
2. takes the server origin from Quarto's `Listening on` line and the
   document-specific path from `Browse at` when both are present;
3. validates the resulting URL as loopback-only HTTP; and
4. probes the URL from the extension host until it responds before installing
   it in the preview panel.

A long initial render remains in startup while Quarto continues producing
output. Raven treats startup as hung only after 120 seconds with no stdout or
stderr activity, and stopping Preview cancels any outstanding readiness probe.

A persistent 404 is treated as a non-browser-previewable output format, and
the panel tells you to use `Raven: Quarto Render` instead.

The rendered page appears in an iframe inside the **Quarto Preview** panel.
Raven does not watch the source file itself. Quarto's built-in watcher owns
refresh: save the `.qmd` file and Quarto re-renders it, after which the served
page reloads itself.

The panel also provides **Open in Browser**, **Stop Preview**, and, after a
stop, failure, restored tab, or unexpected process exit, **Restart Preview**.
Raven never opens an external browser automatically. If VS Code cannot open the
browser, Raven warns and writes the URL to **Raven: Quarto** so you can copy it.

### Stop Quarto Preview

`Raven: Stop Quarto Preview` stops the preview associated with the active
document's project-or-file key. It does not save the file, parse frontmatter,
check workspace trust, or resolve the Quarto executable. Closing a preview
panel also stops the generation owned by that panel.

Stopping is idempotent: a second request while the process is already stopping
does nothing. Raven reports when the preview stopped or when no matching
preview is running. Stop also cancels a Preview command that is still saving,
discovering Quarto, or otherwise completing preflight, before a process starts.

### Quarto Render

`Raven: Quarto Render` saves a dirty document, then runs:

```text
quarto render <file>
```

The command runs in the same project-aware working directory as Preview. It
uses a cancellable progress notification, streams output to **Raven: Quarto**,
and permits only one render at a time for a Quarto project, preventing two
documents from concurrently modifying shared `_site`, `.quarto`, or freeze
state. Standalone files retain independent render slots, with symlink aliases
treated as the same file. It does not start a preview server. The guard is
released when the Quarto process finishes; choosing or dismissing an action in
the outcome notification does not block a subsequent render.

After Quarto exits:

- If Quarto reports an HTML, PDF, or DOCX output path, Raven shows a **Saved**
  notification with format-appropriate open actions. In a remote workspace,
  the external-open action becomes **Download**. **Open in Editor** remains
  available. Relative paths are checked against both the source document's
  directory and the Quarto project working directory.
- For another reported output type, Raven shows a **Rendered** notification
  with **Reveal**.
- If Quarto exits successfully but does not print an output path Raven can
  parse, Raven reports that rendering succeeded and directs you to the output
  channel.
- A non-zero exit shows an error with **Show Output**. A timeout names
  `raven.quarto.render.timeoutMs`. User cancellation is silent after the
  process is stopped.

The Render command never opens the output automatically; you choose an action
from the outcome notification.

### Show Quarto Output

`Raven: Show Quarto Output` opens the **Raven: Quarto** output channel. Preview
startup, Quarto stdout/stderr, frontmatter parse fallbacks, render output, and
panel advisories are logged there. The command is always available from the
Command Palette.

## Apply VS Code theme

The preview toolbar's **Apply VS Code theme** toggle recolors the live Quarto
page to match the active VS Code theme and preview fonts. Switching the toggle,
changing either font setting, or changing the active color theme updates the
open preview without reloading the page or re-rendering the document. The
preference is per-user, is shared by all open Quarto preview panels, and is
persisted across VS Code sessions.

The overlay works with standard Quarto HTML documents, websites, and books.
Raven reapplies it after Quarto's live re-render and as you follow cross-page
navigation within a website or book. Turning the toggle off removes Raven's
overlay and restores the appearance authored by the Quarto document and its
theme. It does not modify the source, generated output, or the page opened by
**Open in Browser**.

The color mapping is intentionally coarse. Raven maps syntax spans into ten
broad roles such as keyword, string, comment, and function, and covers common
Quarto, Pandoc, and Bootstrap surfaces. It aims to make the preview feel at home
in the editor, not to reproduce VS Code's tokenization or every custom Quarto
theme rule exactly. The document surface follows `editor.background`, while
inline and block code use `textCodeBlock.background`, matching VS Code's own
Markdown preview and preserving themes that give code a distinct tint.

At a high level, Raven places a loopback proxy in front of Quarto's local
preview server and injects a small theme bridge into eligible HTML responses.
The preview remains a sandboxed, cross-origin iframe. Only validated,
non-sensitive presentation data—the enabled state, colors, and sanitized font
families—crosses the boundary. If Raven cannot start or probe the proxy, it
falls back to the ordinary unthemed Quarto preview instead of failing the
preview.

### Fonts

The two Quarto font settings accept the same comma-separated form as CSS
`font-family`; quoted names with spaces are valid, for example
`"JetBrains Mono", "Fira Code", monospace`.

When **Apply VS Code theme** is on, Raven resolves each font slot in this order:

1. `raven.quarto.fontFamily` for body/prose or
   `raven.quarto.monospaceFontFamily` for code, when non-empty.
2. `markdown.preview.fontFamily` for body/prose or `editor.fontFamily` for
   code. The monospace fallback honors `[quarto]` language-scoped
   `editor.fontFamily` overrides.
3. A built-in sans-serif or monospace fallback if the configured values are
   invalid.

Both Raven settings are resource-scoped, so folders in a multi-root workspace
can choose different fonts. Changes to either Raven setting or either VS Code
fallback update an open preview live. Raven sanitizes the values before sending
them to the bridge and appends a generic family when needed; invalid CSS-wide
keywords, unsafe characters, unbalanced quotes, and bare parentheses fall
through to the next value in the chain.

## Preview ownership and keying

Raven keys a preview by Quarto project when it finds one, otherwise by source
file:

- Starting at the source file's directory, Raven walks to the filesystem root
  looking for the nearest `_quarto.yml` or `_quarto.yaml`. This walk is not
  bounded by the VS Code workspace.
- If a marker is found, its directory is the preview key and working
  directory. Every `.qmd` in that project shares one Raven preview slot.
- Without a marker, the individual `.qmd` file is the key and its containing
  directory is the working directory.

Different keys can preview concurrently. Previewing another document with the
same key stops the old process and starts a new one; Raven does not retarget a
running Quarto process. Stop from any document with that project key stops the
project's current preview.

Preview processes are owned by the current VS Code window's extension host.
Another window has its own preview registry, so Stop affects only the current
window. If VS Code restores a preview tab after a window reload, Raven shows an
inert placeholder; restoration never starts Quarto or runs document code.
Choose **Restart Preview** when you want to start it again.

This project lookup is only Raven's keying and working-directory heuristic.
Quarto still performs its own project discovery from the target path.

## Security model

Quarto rendering can execute code from the workspace. Raven therefore exposes
Preview and Render only in trusted workspaces and rechecks trust in the command
handlers.

For Preview, Raven starts Quarto with `--host 127.0.0.1` and accepts only an
`http:` URL whose host is exactly `127.0.0.1`, `localhost`, or `[::1]`. URLs
with credentials, non-loopback hosts, malformed ports, or another scheme are
rejected. This matters because document code can write text that resembles
Quarto's own startup messages. Raven then starts its own loopback-only proxy,
fixed to that validated Quarto origin; it cannot be used as an open proxy. The
iframe frames the VS Code-mapped proxy URL, and the proxy forwards HTTP and
live-reload WebSocket traffic to Quarto. If proxy startup fails, Raven falls
back to framing the validated Quarto URL directly without the theme bridge.

The rendered document can contain JavaScript. Raven runs it in a sandboxed,
cross-origin iframe rather than in the Raven-controlled webview document. The
frame permits the capabilities ordinary Quarto output needs, including
scripts, forms, downloads, and same-origin access to its own resources, while
denying capabilities such as top-level navigation and popups. It cannot reach
the outer webview DOM or VS Code API. The injected bridge does not change that
boundary: it accepts an exact, parent-source-checked theme message from the outer
shell and applies only validated colors and sanitized font families.

VS Code maps the validated loopback proxy URL for each new iframe installation,
which also enables remote port forwarding where supported. Raven does not
cache that mapping and never opens a browser on its own. **Open in Browser** is
the explicit escape hatch to Quarto's original URL when a widget, browser
policy, or remote tunnel does not work inside the sandboxed panel.

## Settings

| Setting | Default | Effect |
|---|---:|---|
| `raven.quarto.path` | `""` | Absolute path to a Quarto CLI executable. Leave empty to search `PATH` and the standard platform locations listed above. |
| `raven.quarto.viewerColumn` | `"beside"` | Column for newly created preview panels: `"active"` or `"beside"`. It does not move an existing panel. |
| `raven.quarto.render.timeoutMs` | `600000` | Hard timeout in milliseconds for a one-shot Quarto render. Cancellation and timeout stop the process tree with escalating signals. |
| `raven.quarto.fontFamily` | `""` | Body/prose font for the live-themed preview. Empty inherits `markdown.preview.fontFamily`. |
| `raven.quarto.monospaceFontFamily` | `""` | Monospace font for code in the live-themed preview. Empty inherits `editor.fontFamily`. |

See the [settings reference](settings-reference.md) for the generated schema
summary.

## Troubleshooting

### Start with the output channel

Run `Raven: Show Quarto Output`. The **Raven: Quarto** channel contains the
exact Quarto output and Raven's preview or render diagnostics. It is the first
place to check for missing dependencies, invalid YAML, execution errors, an
unexpected preview exit, or a timeout.

If Quarto is installed but Raven cannot find it, set `raven.quarto.path` to the
absolute executable path on the extension-host machine. Under Remote SSH, a
path on your local machine is not visible to the remote extension host.

### The preview panel is blank

Raven cannot reliably inspect a cross-origin frame to tell whether it rendered
correctly. Eight seconds after installing the frame, the panel shows the
advisory **If the preview looks blank, try Open in Browser.** This banner does
not prove that loading failed; it is an honest fallback for an opaque iframe.

Use **Open in Browser** to open the validated loopback URL through VS Code. For
Remote SSH, Quarto and the readiness probe run remotely, while VS Code maps the
loopback URL back to the client. If the panel stays blank, check the VS Code
**Ports** view and your organization's port-forwarding or proxy policy, then
try **Open in Browser**. Raven does not guarantee preview tunneling in
Codespaces.

### Preview reports HTTP 404

Formats such as DOCX and PPTX can render successfully but cannot be served as a
browser preview. Quarto may still advertise a URL for them; Raven's readiness
probe turns the persistent 404 into a message suggesting `Raven: Quarto
Render`. Render the document and open, download, or reveal the generated file
from the success notification.

## Limitations

- Preview and Render require a trusted workspace.
- Shiny is not supported by this lifecycle. Raven rejects `server: shiny` and
  `server: { type: shiny }` when they appear in the document's own
  frontmatter. This is best-effort: project-level Shiny configuration in
  `_quarto.yml`, `_metadata.yml`, or profiles is not detected before Raven
  invokes Quarto. Shiny requires the separate `quarto serve` lifecycle.
- Browser preview depends on the output format. Non-browser-previewable
  formats such as DOCX and PPTX fail preview with a 404 message; use Render.
- A page with a restrictive `script-src` or `style-src` Content Security
  Policy that permits scripts or styles only by nonce or hash can block the
  injected bridge. The toggle then reports **Theme bridge unavailable on this
  page**.
- RevealJS presentations, heavily customized SCSS, and author-provided
  light/dark theme toggles are themed on a best-effort, coarse basis. Raven
  does not promise exact tokenization or full custom-theme fidelity.
- A remote tunnel mounted under a URL path prefix, rather than mapped by
  authority and port, is a known caveat for redirects. VS Code's usual Remote
  SSH, WSL, and Dev Container port mappings are authority-based.
- PDF and other non-HTML previews are unaffected by **Apply VS Code theme**;
  Raven does not inject or theme those responses.
- Quarto processes are window-owned. Preview and in-flight one-shot render
  processes do not continue through extension deactivation or window reload,
  and Stop does not reach previews owned by another VS Code window.
- Remote SSH uses VS Code's URI mapping and port-forwarding support, but Raven
  does not guarantee that Quarto Preview works in Codespaces.
