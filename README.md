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
--notify       # show the result as a desktop notification
--engine ai    # use an LLM instead of Google
--geometry "X,Y WxH"   # OCR a fixed region instead of dragging one
```

Bound to a key there is no terminal to print to, so use `--notify --copy`.

## Daemon

```sh
wl-translate daemon
```

Runs resident with the tesseract language models loaded, and shows results in a
window instead of a notification. Every verb automatically hands its work to the
daemon when one is running, so the same keybinds get faster and gain a UI with
nothing to change. `--no-daemon` forces the work into the calling process.

It also exposes the verbs on D-Bus, so a compositor can trigger it without this
program's CLI in the loop at all:

```sh
busctl --user call org.wl_translate.Daemon /org/wl_translate/Daemon \
       org.wl_translate.Daemon1 Selection s en
```

That call returns in ~8ms — a keybind never waits for OCR.

Methods: `Ocr(s to)`, `OcrRaw(s to)`, `Selection(s to)`, `Clipboard(s to)`,
`Text(s text, s to)`.

The popup right-aligns right-to-left text and left-aligns everything else, per
side, using the detected source language and the target language.

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
bind = SUPER CTRL, T,     exec, wl-translate selection --to en --copy --notify
bind = SUPER ALT, T,      exec, wl-translate clipboard --to en --copy --notify
bind = SUPER, Print,      exec, wl-translate ocr --to en --copy --notify
bind = SUPER SHIFT, Print, exec, wl-translate ocr --raw --copy --notify
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

- The popup sets no Wayland `app_id`, so window rules have to match on title.
- Source text is not editable yet, so OCR mistakes cannot be corrected in place.
- No language picker in the window; the target language comes from the caller.
- The D-Bus methods carry only a target language. Anything using `--from`,
  `--engine` or `--geometry` runs in the calling process instead of the daemon.

## License

MIT
