# creak

Minimal Wayland layer-shell notification popup. No daemon.

## Build

```
cargo build --release
```

Binary output: ./target/release/creak

## Usage

```
creak [--top-left|--top|--top-right|--left|--center|--right|--bottom-left|--bottom|--bottom-right] [--timeout ms] [--width px] [--font font] [--padding px] [--border-size px] [--border-radius px] [--background #RRGGBB[AA]] [--text #RRGGBB[AA]] [--border #RRGGBB[AA]] [--edge px] [--default-offset px] <title> [body...]
```

Examples:

```
creak "hi"
creak --top-left "title" "body"
creak --bottom "done"
creak --timeout 2000 "short"
creak --width 420 "wide"
creak --background "#00ff00" --text "#000000" "green"
```

## Config

Config file: `$XDG_CONFIG_HOME/creak/config`

The config file is a list of default CLI options (same style as ripgrep). Each line is parsed like shell args; blank lines and lines starting with `#` are ignored.

Example config:

```
# appearance
--font "SimSun 25"
--width 350
--padding 10
--border-size 5
--border-radius 10
--background "#190b10"
--text "#c5c2c3"
--border "#c5c2c3"

# placement
--edge 20
--default-offset 250

# timing
--timeout 5000
```

## Integration

The `alert` wrapper in `/home/oboro/sync/shell/alert` calls `creak`.
