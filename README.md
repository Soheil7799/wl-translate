# wl-translate

On-screen OCR, translation and screenshots for Wayland.

Read text off the screen and translate it, translate what you have highlighted,
or take an annotated screenshot. Bound to whatever keys your compositor uses.

## Why

Wayland gives an application no global hotkeys, and gives a background process
no way to read the selection through a toolkit clipboard API. Most translator
apps were built on X11 assumptions and quietly stopped working — including Crow
Translate, whose shortcut settings use `QHotkey`, which is X11-only and silently
does nothing.

`wl-translate` inverts both:

- **The compositor owns the keybinding.** This program is a set of verbs you
  bind. Switch from Hyprland to KWin to niri and you rewrite one line of keybind
  config; nothing here changes.
- **Selection reads go through `wl-clipboard`**, which uses the
  `wlr-data-control` protocol and works without keyboard focus. A toolkit
  clipboard API cannot — that is the wall Crow hits.

It is GTK4 for one reason above all: Pango implements the Unicode bidirectional
algorithm and Arabic shaping properly, *including caret movement and selection
through mixed right-to-left and left-to-right runs*. This window exists to edit
Persian, not merely to display it.

## Install

```sh
sudo pacman -S gtk4 slurp grim wl-clipboard tesseract hyprpicker

cargo build --release
install -Dm755 target/release/wl-translate ~/.local/bin/wl-translate
```

`hyprpicker` is optional and only used to freeze the screen for the OCR region
drag. Screenshots freeze by capturing up front and need nothing extra.

### Language models

Distributions ship the legacy tessdata. The Persian model in `tessdata_best` is
six times the size and reads screen-resolution Persian far better:

```sh
mkdir -p ~/.local/share/tessdata && cd ~/.local/share/tessdata
curl -LO https://github.com/tesseract-ocr/tessdata_best/raw/main/fas.traineddata
curl -LO https://github.com/tesseract-ocr/tessdata_fast/raw/main/eng.traineddata
curl -LO https://github.com/tesseract-ocr/tessdata_fast/raw/main/ita.traineddata
```

`best` for the language that needs it, `fast` for the ones that do not — `best`
everywhere costs several hundred megabytes of resident memory for no accuracy
you would notice. Then set `tessdata` in the config below.

## Usage

Verbs carry no languages. Which languages you work in is a setting you change in
the window and it is remembered — a keybind should say *what to do*, not restate
what you are translating into.

```sh
wl-translate selection     # translate the current mouse selection
wl-translate clipboard     # translate the clipboard
wl-translate ocr           # drag a region, read it, translate it
wl-translate ocr --raw     # drag a region, just extract the text
wl-translate text "ciao"   # translate an argument
wl-translate show          # raise the window as it is

wl-translate shot          # screenshot: drag a region
wl-translate shot window   # the focused window, still adjustable
wl-translate shot screen   # the focused output, still adjustable

wl-translate daemon        # run resident (see below)
```

Flags exist for scripting and are never needed day to day: `--to`, `--from` and
`--engine` override the saved settings for one run, `--geometry "X,Y WxH"` skips
the drag, `--no-daemon` does the work in the calling process.

## The daemon

Everything is handled by a resident daemon. Verbs hand their work to it and it
starts one if none is running, so a keybind behaves the same either way.

It exposes the verbs on D-Bus, so a compositor can drive it without this
program's CLI in the loop at all:

```sh
busctl --user call org.wl_translate.Daemon /org/wl_translate/Daemon \
       org.wl_translate.Daemon1 Selection
```

That call returns in ~8ms — a keybind never waits for OCR.

Methods, none of which take a language: `Ocr()`, `OcrRaw()`, `Selection()`,
`Clipboard()`, `Text(s)`, `Shot(s mode)`, `Show()`.

Run it under systemd so it comes back if it dies:

```sh
systemctl --user enable --now wl-translate.service
```

It idles at about 6MB. Tesseract's models are loaded on first use and dropped
again after three minutes, so the memory is only held while you are using it.

## The window

Modelled on Crow Translate: a row of language chips per side with `auto` first
and the rest most-recently-used, a swap button between them, and two editable
panes.

- Clicking a chip re-translates immediately; `⋯` opens the full language list.
- Editing the source re-translates after a 350ms pause. Editing the translation
  triggers nothing, so it is a place to adjust wording before copying.
- The clock icon lists recent translations; click one to bring it back.
- `auto` on the source side means "detect it". On the target side there is
  nothing to detect, so it means your system language.
- `Esc` closes it.

Right-to-left text lays out right-to-left, in the same buffer as left-to-right
text, with the caret behaving correctly in both. That is Pango, and it costs no
code here at all.

## Screenshots

The screen is captured up front and shown back fullscreen, so nothing is live:
a video, a scrolling page or a hover tooltip cannot move out from under the
selection. Drag a region on that frozen copy, adjust it by any corner or edge or
drag the whole box, and nothing is committed until you say so.

| key | does |
|---|---|
| `Enter` | save to disk **and** copy |
| `c` | copy only, no file |
| `e` | extract the text and copy it |
| `t` | translate the text in it |
| `Esc` | leave the current tool, or cancel |

### Annotation

| key | tool | | key | tool |
|---|---|---|---|---|
| `v` | select and move | | `h` | highlighter |
| `p` | pen | | `b` | blur |
| `a` | arrow | | `n` | step number |
| `r` | box | | `x` | text |
| `o` | ellipse | | | |

`1`–`6` pick a colour, `w` cycles thickness, `Ctrl+Z` / `Ctrl+Shift+Z` undo and
redo. The toolbars stick to whichever screen edges the selection leaves clear.

Two that are not what they look like:

- **Blur pixelates in place.** A black box drawn on top can be undone by anyone
  holding the file; redaction that can be undone is not redaction.
- **Step numbers count from how many are placed**, not a running total, so undo
  hands the number back rather than leaving a gap.

While typing text the keyboard belongs entirely to the text — the tools are
single letters, so typing "copy" would otherwise pick a colour, save the shot
and close the overlay. `Enter` keeps it, `Shift+Enter` starts a line, `Esc`
discards it.

## Configuration

`~/.config/wl-translate/config.json`, written by the window whenever you change
a language:

```json
{
  "source": "auto",
  "target": "fa",
  "recent_source": ["it", "en", "fa"],
  "recent_target": ["fa", "en", "it"],
  "engine": "google",
  "langs": "fas+ita+eng",
  "freeze": true,
  "tessdata": "/home/you/.local/share/tessdata"
}
```

A missing or unparseable file just means defaults — settings should never be the
reason a keybind stops working.

### Hyprland

```
windowrulev2 = float, class:^(org\.wl_translate\.Gtk)$, title:^(wl-translate)$
windowrulev2 = center, class:^(org\.wl_translate\.Gtk)$, title:^(wl-translate)$
windowrulev2 = size 860 460, class:^(org\.wl_translate\.Gtk)$, title:^(wl-translate)$

bind = SUPER CTRL, C,      exec, wl-translate selection
bind = SUPER SHIFT, C,     exec, wl-translate ocr
bind = , Print,            exec, wl-translate shot region
bind = ALT, Print,         exec, wl-translate shot window
bind = CTRL, Print,        exec, wl-translate shot screen
```

Both windows are one GTK application and share an `app_id`, so rules tell them
apart by title. Do not add a rule for the overlay: it makes itself fullscreen,
and floating it makes GTK fall back to a default size, which stops the selection
lining up with the capture behind it.

## Backends

**`google`** (default) is the undocumented endpoint the Google Translate web
widget calls. No API key, ~105ms. Rate-limited per IP and outside Google's terms
of service, so it suits personal use and should not be a product default.

**`ai`** is any OpenAI-compatible chat endpoint. Better on Persian, idiom, and
text with OCR errors in it, at the cost of a key. Configured by environment so
no provider is baked in:

```sh
export WLT_AI_KEY=...
export WLT_AI_MODEL=...
export WLT_AI_URL=https://api.groq.com/openai/v1   # optional, the default
```

## Notes

Some things that cost time and are easy to trip over again:

- **GTK's GSK renderer initialises Vulkan by default.** On a machine with an
  NVIDIA GPU that pulled in the driver: 235MB resident and two dozen threads for
  a daemon that shows nothing most of its life. The app sets
  `GSK_RENDERER=cairo` unless you set it yourself.
- **Tesseract is built against OpenMP** and spawns a thread per core. For one
  screenshot-sized image that buys nothing and costs a dozen threads plus their
  allocator arenas. Pinned to one.
- **Dropping the OCR engine does not return memory** on its own — glibc keeps
  freed heap in its arenas, so RSS never moves. `malloc_trim` is what hands the
  pages back.
- **The capture is drawn once, as a texture.** Only the dimming and annotations
  are redrawn per frame. Drawing the capture every frame is what made an earlier
  version stutter, and no renderer choice fixes that much work per pointer event.

## Development

Geometry (`geom.rs`) and annotation shapes (`annotate.rs`) hold no widget types.
That is deliberate: they survived a complete change of toolkit unchanged, tests
and all, because the rules for what a drag means and what shape an arrow head is
have nothing to do with which library draws them.

```sh
cargo test
```

## License

MIT
