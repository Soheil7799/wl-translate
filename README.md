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

```sh
wl-translate ocr --to en            # drag a region, OCR it, translate
wl-translate ocr --raw              # drag a region, just extract the text
wl-translate selection --to fa      # translate the current mouse selection
wl-translate clipboard --to it      # translate the clipboard
wl-translate text --to en "ciao"    # translate an argument

# useful flags
--from it      # skip language detection
--copy         # also put the result on the clipboard
--engine ai    # use an LLM instead of Google
```

### Keybinds

Hyprland:

```
bind = SUPER, T, exec, wl-translate selection --to en --copy
bind = SUPER SHIFT, T, exec, wl-translate ocr --to en --copy
bind = SUPER ALT, T, exec, wl-translate ocr --raw --copy
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

Working: `selection`, `clipboard`, `text`, `ocr`.
Planned: resident daemon with preloaded tesseract, D-Bus verbs, and an iced GUI
popup with editable source text and language pickers.

## License

MIT
