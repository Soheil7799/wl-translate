# wl-translate

On-screen OCR and translation for Wayland.

Select a region of the screen, read the text in it, translate it. Or translate
whatever you have highlighted with the mouse. Bound to whatever keys your
compositor uses.

## Why

Wayland gives no application global hotkeys, and gives a background process no
way to read the selection through a toolkit clipboard API. Most translator apps
were built on X11 assumptions and quietly stopped working:

- `QHotkey` and friends are X11-only, so in-app shortcut settings silently no-op.
- A toolkit's clipboard read needs keyboard focus, which a tray daemon does not
  have.

`wl-translate` inverts both. The **compositor owns the keybinding** and this
program is just a set of verbs you bind. Selection reads go through
`wl-clipboard`, which uses the `wlr-data-control` protocol and works without
focus. Switch from Hyprland to KWin to niri and you rewrite one line of keybind
config; nothing in this program changes.

## Install

Requires `slurp`, `grim`, `wl-clipboard`, and `tesseract` with the language data
you want:

```sh
sudo pacman -S slurp grim wl-clipboard tesseract \
               tesseract-data-eng tesseract-data-ita tesseract-data-fas

cargo build --release
install -Dm755 target/release/wl-translate ~/.local/bin/wl-translate
```

## Usage

Verbs carry no languages. Which languages you work in is a setting you change in
the window and it is remembered — a keybind should say *what to do*, not restate
what you are translating into.

```sh
wl-translate selection     # translate the current mouse selection
wl-translate clipboard     # translate the clipboard
wl-translate ocr           # drag a region, OCR it, translate
wl-translate ocr --raw     # drag a region, just extract the text
wl-translate text "ciao"   # translate an argument
wl-translate show          # raise the window as it is

wl-translate shot          # drag a region and review it
wl-translate shot window   # pick a window
wl-translate shot screen   # the focused output
```

## Screenshots

The screen is held still while you drag, so a video or a scrolling page cannot
move out from under the selection. What you capture is then shown for review and
nothing is committed until you say so:

| key | does |
|---|---|
| `Enter` / `Space` | save to disk **and** copy |
| `Ctrl+C` | copy only, no file |
| `Esc` | discard |

Files land in `<pictures>/Screenshots/Screenshot_<timestamp>.png`.

With no daemon running there is no window to review in, so a shot copies and
saves immediately instead.

Flags exist for scripting and are never needed day to day:

```sh
--to fa --from it      # override the saved languages for one run
--engine ai            # use an LLM instead of Google
--geometry "X,Y WxH"   # OCR a fixed region instead of dragging one
--copy --notify        # clipboard and notification, for running without a daemon
--no-daemon            # do the work here even if a daemon is running
```

## Daemon

```sh
wl-translate daemon
```

Runs resident with the tesseract language models loaded and shows results in a
window. Every verb hands its work to the daemon when one is running, so the same
keybinds get faster and gain a UI with nothing to change.

It exposes the verbs on D-Bus, so a compositor can trigger it without this
program's CLI in the loop at all:

```sh
busctl --user call org.wl_translate.Daemon /org/wl_translate/Daemon \
       org.wl_translate.Daemon1 Selection
```

That call returns in ~8ms — a keybind never waits for OCR.

Methods, none of which take a language: `Ocr()`, `OcrRaw()`, `Selection()`,
`Clipboard()`, `Text(s text)`, `Show()`.

### The window

Modelled on Crow Translate: a row of language chips per side with `auto` first
and the rest ordered most-recently-used, a swap button between them, and two
editable panes.

- Clicking a chip re-translates the current text immediately.
- Editing the source re-translates after a 350ms pause.
- The translation pane is editable too, so you can adjust wording before
  copying; editing it triggers nothing.
- `auto` on the source side means "detect it". On the target side there is
  nothing to detect, so it means your system language, taken from the locale.
- Right-to-left text aligns right and left-to-right aligns left, handled by
  cosmic-text without any special casing.

## Settings

`~/.config/wl-translate/config.json`, written by the window whenever you change
a language:

```json
{
  "source": "auto",
  "target": "fa",
  "recent_source": ["it", "en", "fa"],
  "recent_target": ["fa", "en", "it"],
  "engine": "google",
  "langs": "eng+ita+fas"
}
```

A missing or unparseable file just means defaults — settings should never be the
reason a keybind stops working.

### Window rule

The popup has no `app_id` yet, so match it on title:

```
windowrulev2 = float, title:^wl-translate$
windowrulev2 = size 720 440, title:^wl-translate$
windowrulev2 = center, title:^wl-translate$
```

Without this the compositor tiles it like an ordinary window.

### Keybinds

Hyprland:

```
bind = SUPER CTRL, T,     exec, wl-translate selection
bind = SUPER ALT, T,      exec, wl-translate clipboard
bind = SUPER, Print,      exec, wl-translate ocr
bind = SUPER SHIFT, Print, exec, wl-translate ocr --raw
```

KDE, niri, sway and anything else: bind the same commands. That is the whole
portability story.

## Backends

**`google`** (default) is the undocumented endpoint the Google Translate web
widget calls. No API key, no account, ~105ms. It is rate-limited per IP and
outside Google's terms of service, so it suits personal use and should not be a
product default.

**`ai`** is any OpenAI-compatible chat endpoint. Noticeably better on Persian,
idiom, and text with OCR errors in it, at the cost of a key and a slower round
trip. Configured entirely by environment so no provider is baked in:

```sh
export WLT_AI_KEY=...
export WLT_AI_MODEL=...
export WLT_AI_URL=https://api.groq.com/openai/v1   # optional, this is the default
```

## Performance

Capture is deliberately two steps — pick the region first, then grab only that
region as PPM. Measured on a three-monitor setup:

| approach | time |
|---|---|
| whole desktop → PNG, crop after | ~420 ms |
| region only → PPM | ~42 ms |

The rest of the budget is ~160 ms for tesseract and ~105 ms for the translation
round trip.

## Development

Worktrees share one build cache via `.cargo/config.toml`, which is machine-local
and gitignored because it holds an absolute path. Recreate it after cloning:

```sh
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[build]
target-dir = "/absolute/path/to/a/shared/cache"
EOF
```

Without it every `git worktree add` recompiles the full dependency tree from
scratch, which makes worktrees slower than switching branches.

## Status

Working: `selection`, `clipboard`, `text`, `ocr`, `daemon` with the D-Bus verbs
and the popup.

Known gaps:

- The window sets `id` but the Wayland `app_id` still comes through empty, so
  compositor rules have to match on title.
- No full language list — only the chips, so reaching a language you have not
  used recently means editing the config file.
- Overriding `--from`, `--to`, `--engine` or `--geometry` runs in the calling
  process rather than the daemon, since the D-Bus verbs carry no arguments.
- The `ai` backend is written but untested.

## License

MIT
