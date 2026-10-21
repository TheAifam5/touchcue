#!/bin/sh
# Captures PNG and animated GIF scenarios in a nested Hyprland session with a
# private Wayland socket and session bus. Only the nested windows appear on
# the host desktop. Scenarios named anim-* produce GIFs; others produce PNG images.
#
# Usage: scripts/screenshots.sh [--dry-run] [--headless] [SCENARIO...]
#
#   --dry-run  print the plan and the generated configuration; start nothing
#   --headless capture headless outputs instead of the nested Hyprland windows;
#              fails on drivers that cannot allocate their buffers, such as NVIDIA
#   SCENARIO   only these scenarios; default: every scenario of the example
#
# Needs a running Wayland session, the example built with
# `cargo build -p touchcue-ui --example screenshots`, and Hyprland, hyprctl,
# dbus-run-session, grim, jq, magick (ImageMagick), ps and mkfifo; mako for the
# notification scenario; ffmpeg and a date that prints nanoseconds with %N for
# the anim-* scenarios.
set -eu
# Words are split from scenario names and hyprctl output but never globbed.
set -f

# Size of every headless output, in pixels at scale 1; window outputs take the
# size the host gives their windows.
WIDTH=1280
HEIGHT=720
# Background of the nested outputs in full-output captures and in animated
# GIFs, as RRGGBB.
BACKGROUND=8a8f99
# Limits for the nested session, in tenths of a second.
START_LIMIT=200
READY_LIMIT=150
LAYER_LIMIT=100
STOP_LIMIT=50
# Pause between a layer appearing and its capture, in seconds.
SETTLE=0.5
# Frames per second the animated GIFs are recorded at, the longest recording
# in seconds, and the largest GIF width in pixels and size in bytes.
GIF_FPS=25
GIF_LIMIT=30
GIF_MAX_WIDTH=480
GIF_MAX_BYTES=1048576
HEADLESS=SHOT
SECOND=SHOT2

die() {
	printf 'screenshots: %s\n' "$*" >&2
	exit 1
}

usage() {
	cat <<EOF
Usage: scripts/screenshots.sh [--dry-run] [--headless] [SCENARIO...]

  --dry-run  print the plan and the generated configuration; start nothing
  --headless capture headless outputs instead of the nested Hyprland windows;
             fails on drivers that cannot allocate their buffers, such as NVIDIA
  SCENARIO   only these scenarios; default: every scenario of the example

Scenarios named anim-* are recorded as animated GIFs, the others captured as
PNG images.
EOF
}

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
example=${CARGO_TARGET_DIR:-$repo/target}/debug/examples/screenshots
out_dir=$repo/docs/public/screenshots
work_root=$repo/target/screenshots

dry_run=false
window=true
filter=
for arg in "$@"; do
	case $arg in
	--dry-run) dry_run=true ;;
	--headless) window=false ;;
	-h | --help)
		usage
		exit 0
		;;
	-*) die "unknown option $arg; see --help" ;;
	*) filter="$filter $arg" ;;
	esac
done
host_bus=${DBUS_SESSION_BUS_ADDRESS:-}
# Only host_hctl reaches the host Hyprland; every other hyprctl call names the
# nested instance.
host_sig=${HYPRLAND_INSTANCE_SIGNATURE:-}
unset HYPRLAND_INSTANCE_SIGNATURE
# Nested clients use the nested Wayland socket. The nested compositor does not
# inherit Aquamarine backend or DRM-device overrides.
unset WAYLAND_SOCKET AQ_BACKENDS AQ_DRM_DEVICES

need() {
	command -v "$1" >/dev/null 2>&1 || die "$1 is not installed; it comes from the system packages"
}

[ -x "$example" ] || die "$example is missing; run: cargo build -p touchcue-ui --example screenshots"
all=$("$example" --list) || die "the example failed to list its scenarios"
if [ -z "$filter" ]; then
	selected=$all
else
	selected=
	for name in $filter; do
		printf '%s\n' "$all" | grep -Fqx -- "$name" || die "unknown scenario $name; known: $(printf '%s ' $all)"
		selected="$selected $name"
	done
fi
needs_mako=false
needs_gif=false
for name in $selected; do
	case $name in
	notification) needs_mako=true ;;
	anim-*) needs_gif=true ;;
	esac
done

for tool in Hyprland hyprctl dbus-run-session grim jq magick ps mkfifo mktemp; do
	need "$tool"
done
if $needs_mako; then
	need mako
	need makoctl
fi
if $needs_gif; then
	need ffmpeg
	need timeout
	case $(date +%s%N) in
	*[!0-9]*) die "date does not print nanoseconds with %N; the anim-* scenarios need it" ;;
	esac
fi

# Hyprland's name for the first nested window output; the real name is read
# once the session runs.
window_output=WL-1
if $window; then
	shot_output=$window_output
	second_kind=wayland
	window_position=0x0
	# Hyprland places the second window output automatically to the right.
	second_position=auto-right
else
	second_position=${WIDTH}x0
	shot_output=$HEADLESS
	second_kind=headless
	# The nested window starts as the only output; it moves out of the
	# captured area and is disabled once the headless output exists.
	window_position=0x$((HEIGHT * 4))
fi

# Prints the monitor rule of the nested window output $1, with the Lua
# fields in $2 appended.
window_rule() {
	printf 'hl.monitor({ output = "%s", mode = "%sx%s", position = "%s", scale = 1%s })\n' \
		"$1" "$WIDTH" "$HEIGHT" "$window_position" "$2"
}

background=$BACKGROUND

# Prints the nested Hyprland Lua configuration with $1 as the rule of the
# nested window output.
hyprland_config() {
	cat <<EOF
-- Generated by scripts/screenshots.sh for an isolated screenshot session.
$1
hl.monitor({ output = "$HEADLESS", mode = "${WIDTH}x${HEIGHT}@60", position = "0x0", scale = 1 })
hl.monitor({ output = "$SECOND", mode = "${WIDTH}x${HEIGHT}@60", position = "$second_position", scale = 1 })
hl.monitor({ output = "", mode = "preferred", position = "auto", scale = 1 })

hl.config({
    animations = { enabled = false },
    cursor = { invisible = true },
    ecosystem = { no_update_news = true, no_donation_nag = true },
    misc = {
        disable_hyprland_logo = true,
        disable_splash_rendering = true,
        force_default_wallpaper = 0,
        background_color = "rgb($background)",
        disable_watchdog_warning = true,
        disable_autoreload = true,
    },
    xwayland = { enabled = false },
})
EOF
}

capture_mode() {
	case $1 in
	modal-* | corner) echo output ;;
	all-outputs) echo all ;;
	anim-*) echo gif ;;
	*) echo crop ;;
	esac
}

# Prints the file name of the image of scenario $1.
image_of() {
	case $1 in
	anim-*) echo "$1.gif" ;;
	*) echo "$1.png" ;;
	esac
}

if $dry_run; then
	printf 'example:    %s\n' "$example"
	printf 'work dir:   %s/run.XXXXXX\n' "$work_root"
	printf 'images:     %s\n' "$out_dir"
	printf 'outputs:    %s, plus %s %s for all-outputs\n' "$shot_output" "$second_kind" "$SECOND"
	printf 'mako:       %s\n' "$needs_mako"
	printf 'gifs:       %s fps over #%s, at most %s px wide and %s bytes, %s s long\n' \
		"$GIF_FPS" "$BACKGROUND" "$GIF_MAX_WIDTH" "$GIF_MAX_BYTES" "$GIF_LIMIT"
	printf 'start:      HYPRLAND_NO_SD_VARS=1 HYPRLAND_NO_SD_NOTIFY=1 HYPRLAND_NO_CRASHREPORTER=1 dbus-run-session --config-file=<work>/bus.conf -- Hyprland -c <work>/hyprland.lua\n'
	for name in $selected; do
		printf 'scenario:   %-15s capture %-6s -> %s/%s\n' "$name" "$(capture_mode "$name")" "$out_dir" "$(image_of "$name")"
	done
	printf -- '--- hyprland.lua\n'
	hyprland_config "$(window_rule "$window_output" '')"
	exit 0
fi

[ -n "${XDG_RUNTIME_DIR:-}" ] || die "XDG_RUNTIME_DIR is not set"
# A live host Wayland display is required for nested operation.
[ -n "${WAYLAND_DISPLAY:-}" ] || die "WAYLAND_DISPLAY is not set; run this inside a Wayland session"
case $WAYLAND_DISPLAY in
/*) host_socket=$WAYLAND_DISPLAY ;;
*) host_socket=$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY ;;
esac
[ -S "$host_socket" ] || die "WAYLAND_DISPLAY does not name a Wayland socket: $host_socket"
# A socket left by a crashed compositor passes -S; only a live one answers a
# one-pixel capture.
grim -g "0,0 1x1" -t ppm - >/dev/null 2>&1 || die "the Wayland display $WAYLAND_DISPLAY does not respond"
case $XDG_RUNTIME_DIR in
*'<'* | *'>'* | *'&'* | *'"'* | *"'"*) die "XDG_RUNTIME_DIR holds characters the bus configuration cannot carry" ;;
esac
host_display=$WAYLAND_DISPLAY

work=
rule=
drs_pid=
hypr_pid=
sig=
wl=
bus=
mako_pid=
example_pid=
daemon_pid=

# Reports whether $1 is a running process; a zombie counts as stopped.
alive() {
	kill -0 "$1" 2>/dev/null || return 1
	case $(ps -o stat= -p "$1" 2>/dev/null) in
	Z*) return 1 ;;
	*) return 0 ;;
	esac
}

# Prints the command name of process $1, or nothing when it does not exist.
comm_of() {
	cat "/proc/$1/comm" 2>/dev/null || true
}

# Reports whether process $1 has command name $2; an empty $2 matches any.
named() {
	[ -z "$2" ] || [ "$(comm_of "$1")" = "$2" ]
}

# Sends SIGTERM, waits up to STOP_LIMIT, and sends SIGKILL if needed.
# Processes whose command name differs from $2 are not signaled.
stop() {
	alive "$1" && named "$1" "${2:-}" || return 0
	kill -TERM "$1" 2>/dev/null || true
	i=0
	while alive "$1" && [ "$i" -lt "$STOP_LIMIT" ]; do
		sleep 0.1
		i=$((i + 1))
	done
	if alive "$1" && named "$1" "${2:-}"; then
		printf 'screenshots: process %s ignored SIGTERM; killing it\n' "$1" >&2
		kill -KILL "$1" 2>/dev/null || true
	fi
}

# Reports whether process $1 is a child of process $2.
child_of() {
	[ "$(ps -o ppid= -p "$1" 2>/dev/null | tr -d ' ')" = "$2" ]
}

# Prints the pid of the child of $1 with command name $2.
child_named() {
	ps -o pid=,comm= --ppid "$1" 2>/dev/null | awk -v name="$2" '$2 == name { print $1; exit }'
}

# Runs hyprctl against the host Hyprland.
host_hctl() {
	env HYPRLAND_INSTANCE_SIGNATURE="$host_sig" hyprctl -i "$host_sig" "$@"
}

# Floats and resizes the nested Hyprland's host windows to WIDTH x HEIGHT.
size_windows() {
	if [ -z "$host_sig" ] || ! valid_sig "$host_sig"; then
		printf 'screenshots: the host is not Hyprland; window outputs keep the size the host gives them\n' >&2
		return 0
	fi
	alive "$hypr_pid" && named "$hypr_pid" Hyprland || die "the nested Hyprland is gone"
	addresses=$(host_hctl -j clients | jq -r --argjson pid "$hypr_pid" '.[] | select(.pid == $pid) | .address')
	for address in $addresses; do
		case $address in
		0x*[!0-9a-f]* | 0x) die "unexpected host window address" ;;
		0x*) ;;
		*) die "unexpected host window address" ;;
		esac
		host_hctl dispatch "hl.dsp.window.float({ window = \"address:$address\", action = \"enable\" })" >>"$work/hyprctl.log" 2>&1 ||
			die "floating the nested window failed; see $work/hyprctl.log"
		host_hctl dispatch "hl.dsp.window.resize({ window = \"address:$address\", x = $WIDTH, y = $HEIGHT })" >>"$work/hyprctl.log" 2>&1 ||
			die "resizing the nested window failed; see $work/hyprctl.log"
	done
	poll "$START_LIMIT" outputs_sized || die "the nested outputs did not take ${WIDTH}x$HEIGHT; see $work/hyprctl.log"
}

# Reports whether every nested output is WIDTH x HEIGHT.
outputs_sized() {
	hctl -j monitors | jq -e --argjson w "$WIDTH" --argjson h "$HEIGHT" 'all(.[]; .width == $w and .height == $h)' >/dev/null
}

# Runs hyprctl against the nested instance only.
hctl() {
	env HYPRLAND_INSTANCE_SIGNATURE="$sig" hyprctl -i "$sig" "$@"
}

valid_sig() {
	case $1 in
	'' | *[!A-Za-z0-9_]*) return 1 ;;
	esac
}

valid_wl() {
	case $1 in
	wayland-*) ;;
	*) return 1 ;;
	esac
	case ${1#wayland-} in
	'' | *[!0-9]*) return 1 ;;
	esac
}

# Runs "$2..." every tenth of a second until it succeeds or $1 tenths pass.
poll() {
	limit=$1
	shift
	i=0
	until "$@"; do
		[ "$i" -lt "$limit" ] || return 1
		sleep 0.1
		i=$((i + 1))
	done
}

cleanup() {
	status=$?
	trap '' HUP INT TERM
	set +e
	exec 3>&-
	[ -n "$example_pid" ] && stop "$example_pid"
	[ -n "$mako_pid" ] && stop "$mako_pid"
	# An interrupted startup may have written the pid without finding the instance.
	if [ -z "$hypr_pid" ] && [ -n "$work" ] && [ -s "$work/hyprland.pid" ]; then
		candidate=$(cat "$work/hyprland.pid")
		case $candidate in
		'' | *[!0-9]*) ;;
		*)
			if named "$candidate" Hyprland; then
				hypr_pid=$candidate
			elif [ -n "$drs_pid" ] && named "$candidate" sh && child_of "$candidate" "$drs_pid"; then
				# The launch shell has not executed Hyprland yet; the pid is still ours.
				stop "$candidate"
				hypr_pid=$candidate
			fi
			;;
		esac
	fi
	if [ -z "$daemon_pid" ] && [ -n "$drs_pid" ] && alive "$drs_pid"; then
		daemon_pid=$(child_named "$drs_pid" dbus-daemon)
	fi
	if [ -n "$sig" ]; then
		hctl dispatch 'hl.dsp.exit()' >/dev/null 2>&1
		[ -n "$hypr_pid" ] && poll "$STOP_LIMIT" eval '! alive "$hypr_pid"'
	fi
	[ -n "$hypr_pid" ] && stop "$hypr_pid" Hyprland
	[ -n "$drs_pid" ] && poll "$STOP_LIMIT" eval '! alive "$drs_pid"'
	[ -n "$drs_pid" ] && stop "$drs_pid"
	[ -n "$daemon_pid" ] && stop "$daemon_pid" dbus-daemon
	for pid in $example_pid $mako_pid $hypr_pid $drs_pid $daemon_pid; do
		if alive "$pid"; then
			printf 'screenshots: process %s is still running\n' "$pid" >&2
			status=1
		fi
	done
	# The bus socket is removed only after a known daemon PID has stopped.
	if [ -n "$bus" ] && [ -n "$daemon_pid" ] && ! alive "$daemon_pid"; then
		path=${bus#unix:path=}
		path=${path%%,*}
		case $path in
		"$XDG_RUNTIME_DIR"/dbus-*)
			case ${path#"$XDG_RUNTIME_DIR"/} in
			*/*) ;;
			*) [ -S "$path" ] && [ ! -L "$path" ] && rm -f -- "$path" ;;
			esac
			;;
		esac
	fi
	if valid_sig "$sig" && { [ -z "$hypr_pid" ] || ! alive "$hypr_pid"; }; then
		dir=$XDG_RUNTIME_DIR/hypr/$sig
		if [ -d "$dir" ] && [ ! -L "$dir" ]; then
			rm -rf -- "$dir"
		fi
		if valid_wl "$wl" && [ "$wl" != "$host_display" ]; then
			for path in "$XDG_RUNTIME_DIR/$wl" "$XDG_RUNTIME_DIR/$wl.lock"; do
				if [ -e "$path" ] && [ ! -L "$path" ]; then
					rm -f -- "$path"
				fi
			done
		fi
	fi
	if [ -n "$work" ]; then
		if [ "$status" -eq 0 ]; then
			rm -rf -- "$work"
		else
			printf 'screenshots: logs kept in %s\n' "$work" >&2
		fi
	fi
	exit "$status"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p -- "$work_root" "$out_dir"
work=$(mktemp -d "$work_root/run.XXXXXX")
chmod 700 "$work"

hyprland_config "$(window_rule "$window_output" '')" >"$work/hyprland.lua"
# The private bus has no service directories and does not activate services.
cat >"$work/bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=$XDG_RUNTIME_DIR</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
EOF

(
	unset DISPLAY HYPRLAND_INSTANCE_SIGNATURE
	export HYPRLAND_NO_SD_VARS=1 HYPRLAND_NO_SD_NOTIFY=1 HYPRLAND_NO_CRASHREPORTER=1
	cd "$work"
	# shellcheck disable=SC2016 # expanded by the inner shell
	exec dbus-run-session --config-file="$work/bus.conf" -- sh -c '
		printf "%s\n" "$$" >"$1/hyprland.pid"
		printf "%s\n" "$DBUS_SESSION_BUS_ADDRESS" >"$1/bus"
		exec Hyprland -c "$1/hyprland.lua"
	' sh "$work"
) >"$work/hyprland.log" 2>&1 &
drs_pid=$!

# Sets sig and wl from the instance whose pid is the nested Hyprland's.
find_instance() {
	alive "$drs_pid" || die "nested Hyprland exited; see $work/hyprland.log"
	[ -s "$work/hyprland.pid" ] || return 1
	pid=$(cat "$work/hyprland.pid")
	case $pid in
	'' | *[!0-9]*) die "unexpected nested Hyprland pid" ;;
	esac
	# hypr_pid is set only once the pid runs Hyprland; until then cleanup
	# recovers the launch shell from the pid file.
	found=$(hyprctl instances -j 2>/dev/null |
		jq -r --argjson pid "$pid" '.[] | select(.pid == $pid) | "\(.instance) \(.wl_socket)"') || return 1
	[ -n "$found" ] || return 1
	valid_sig "${found% *}" || die "unexpected instance signature"
	valid_wl "${found#* }" || die "unexpected Wayland socket name"
	named "$pid" Hyprland || return 1
	hypr_pid=$pid
	sig=${found% *}
	wl=${found#* }
	hctl -j monitors >/dev/null 2>&1
}
poll "$START_LIMIT" find_instance || die "nested Hyprland did not start; see $work/hyprland.log"
daemon_pid=$(child_named "$drs_pid" dbus-daemon)
[ -n "$daemon_pid" ] || die "the private bus daemon was not found"
address=$(cat "$work/bus")
case $address in
unix:*) ;;
*) die "the private bus address is not a unix socket address" ;;
esac
[ "$address" != "$host_bus" ] || die "the private bus address equals the host bus address"
bus=$address
printf 'screenshots: nested Hyprland %s on %s\n' "$sig" "$wl" >&2

# Creates output $2 of backend $1 and waits until it is active.
create_output() {
	hctl output create "$1" "$2" >>"$work/hyprctl.log" 2>&1 ||
		die "creating output $2 failed; see $work/hyprctl.log"
	poll "$START_LIMIT" has_output "$2" || die "output $2 did not appear; see $work/hyprctl.log"
}

# Prints "x0 y0 x1 y1", the bounding box of the outputs named in $1.
output_box() {
	hctl -j monitors | jq -r --arg names "$1" '
		($names | split(" ")) as $wanted
		| [.[] | select(.name as $n | $wanted | index($n))]
		| if length == 0 then empty
		  else "\(map(.x) | min) \(map(.y) | min) \(map(.x + .width) | max) \(map(.y + .height) | max)"
		  end'
}

has_output() {
	hctl -j monitors | jq -e --arg name "$1" 'any(.[]; .name == $name)' >/dev/null
}

# Fails when Hyprland reports configuration errors.
check_config() {
	errors=$(hctl -j configerrors) || die "hyprctl configerrors failed"
	printf '%s' "$errors" | jq -e 'type == "array" and all(.[]; . == "")' >/dev/null ||
		die "the nested Hyprland reports configuration errors: $errors"
}

# Rewrites the configuration with $1 as the nested window rule and reloads it.
reload_config() {
	rule=$1
	hyprland_config "$1" >"$work/hyprland.lua"
	hctl reload >>"$work/hyprctl.log" 2>&1 ||
		die "reloading the configuration failed; see $work/hyprctl.log"
	check_config
}

# Sets the background of the nested outputs to $1, as RRGGBB.
set_background() {
	background=$1
	reload_config "$rule"
	sleep "$SETTLE"
}

check_config
window_output=$(hctl -j monitors | jq -r 'if length == 1 then .[0].name else empty end')
case $window_output in
'' | *[!A-Za-z0-9_-]*) die "expected one output with a plain name at start, found: $window_output" ;;
esac
if $window; then
	shot_output=$window_output
	reload_config "$(window_rule "$window_output" '')"
	size_windows
else
	create_output headless "$HEADLESS"
	reload_config "$(window_rule "$window_output" ', disabled = true')"
	if ! poll "$START_LIMIT" eval '! has_output "$window_output"'; then
		printf 'screenshots: %s is still enabled; it lies outside the captured area\n' "$window_output" >&2
	fi
fi

if $needs_mako; then
	cat >"$work/mako.conf" <<EOF
output=$shot_output
default-timeout=0
icons=1
max-icon-size=48
EOF
	(
		unset DISPLAY
		export WAYLAND_DISPLAY="$wl" DBUS_SESSION_BUS_ADDRESS="$bus"
		exec mako -c "$work/mako.conf"
	) </dev/null >"$work/mako.log" 2>&1 &
	mako_pid=$!
	mako_ready() {
		alive "$mako_pid" || die "mako exited; see $work/mako.log"
		DBUS_SESSION_BUS_ADDRESS=$bus makoctl list >/dev/null 2>&1
	}
	poll "$START_LIMIT" mako_ready || die "mako did not start; see $work/mako.log"
fi

# Prints "x0 y0 x1 y1", the bounding box of the mapped layers of namespace $1.
layer_box() {
	hctl -j layers | jq -r --arg ns "$1" '
		[.[].levels[][] | select(.namespace == $ns and .w > 0 and .h > 0)]
		| if length == 0 then empty
		  else "\(map(.x) | min) \(map(.y) | min) \(map(.x + .w) | max) \(map(.y + .h) | max)"
		  end'
}

has_layer() {
	[ -n "$(layer_box "$1")" ]
}

ready_seen() {
	alive "$example_pid" || die "the example exited early; see $work/$name.err"
	grep -q '^ready ' "$work/$name.out"
}

# Prints the grim geometry of the mapped layers of namespace $1, or fails
# when there are none.
layer_geometry() {
	box=$(layer_box "$1")
	[ -n "$box" ] || return 1
	# shellcheck disable=SC2086 # box holds four numbers
	set -- $box
	printf '%s,%s %sx%s\n' "$1" "$2" $(($3 - $1)) $(($4 - $2))
}

# Records geometry $3 for scenario $1 into the animated GIF $2. Writing a line
# to the example's stdin starts its state changes; frames are taken at
# GIF_FPS until it prints done. Each frame lasts until the next one was
# taken, rounded to hundredths of a second, the unit of GIF delays; a frame
# taken under two hundredths after the last kept one is dropped. Frames show
# the popup over BACKGROUND.
record() {
	frames=$work/$1.frames
	mkdir -- "$frames"
	interval=$((1000000000 / GIF_FPS))
	limit=$((GIF_LIMIT * 1000000000))
	n=0
	start=$(date +%s%N)
	printf 'start\n' >&3
	until grep -qx 'done' "$work/$1.out"; do
		alive "$example_pid" || die "$1: the example exited during the recording; see $work/$1.err"
		now=$(date +%s%N)
		[ $((now - start)) -lt "$limit" ] || die "$1: the example did not print done within $GIF_LIMIT s"
		n=$((n + 1))
		printf '%s\n' "$now" >>"$frames/times"
		remaining=$((limit - (now - start)))
		seconds=$((remaining / 1000000000))
		nanos=$((remaining % 1000000000))
		WAYLAND_DISPLAY=$wl timeout -s KILL "$seconds.$(printf '%09d' "$nanos")s" \
			grim -t ppm -g "$3" "$(printf '%s/%05d.ppm' "$frames" "$n")" ||
			die "$1: frame capture failed or exceeded the recording deadline"
		pause=$((start + n * interval - $(date +%s%N)))
		[ "$pause" -le 0 ] || sleep "0.$(printf '%09d' "$pause")"
	done
	end=$(date +%s%N)
	[ "$n" -gt 0 ] || die "$1: the example printed done before the first frame"
	# The per-file framerate keeps the durations in hundredths; the option
	# directive needs -safe 0. The last frame's delay is final_delay.
	awk -v end="$end" -v final="$frames/final" '
		function cs(t) { return int((t - first) / 1e7 + 0.5) }
		function emit(d) { printf "file %s\noption framerate 100\nduration %d.%02d\n", f, d / 100, d % 100 }
		NR == 1 {
			first = $1
			print "ffconcat version 1.0"
		}
		{ c = cs($1) }
		NR == 1 || c - kept >= 2 {
			if (NR > 1) emit(c - kept)
			kept = c
			f = sprintf("%05d.ppm", NR)
		}
		END {
			d = cs(end) - kept
			if (d < 2)
				d = 2
			emit(d)
			print d > final
		}
	' "$frames/times" >"$frames/list"
	ffmpeg -hide_banner -loglevel error -nostdin -y -f concat -safe 0 -i "$frames/list" \
		-filter_complex "scale=w='min(iw\\,$GIF_MAX_WIDTH)':h=-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=full[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle" \
		-fps_mode passthrough -loop 0 -final_delay "$(cat "$frames/final")" "$2" 2>"$work/$1.ffmpeg.log" ||
		die "$1: encoding the GIF failed; see $work/$1.ffmpeg.log"
	size=$(wc -c <"$2" | tr -d ' ')
	[ "$size" -le "$GIF_MAX_BYTES" ] || die "$1: $2 has $size bytes, more than GIF_MAX_BYTES"
}

# Captures scenario $1 whose layers have namespace $2 into $3.
capture() {
	case $(capture_mode "$1") in
	output) WAYLAND_DISPLAY=$wl grim -l 9 -o "$shot_output" "$3" ;;
	all)
		box=$(output_box "$shot_output $SECOND")
		[ -n "$box" ] || die "$1: the outputs disappeared before the capture"
		# shellcheck disable=SC2086 # box holds four numbers
		set -- "$3" $box
		WAYLAND_DISPLAY=$wl grim -l 9 -g "$2,$3 $(($4 - $2))x$(($5 - $3))" "$1"
		;;
	crop)
		geometry=$(layer_geometry "$2") || die "$1: the $2 layer disappeared before the capture"
		set -- "$3" "$geometry"
		# The popups have transparent corners: their alpha is recovered from
		# captures over black and over white.
		set_background 000000
		WAYLAND_DISPLAY=$wl grim -g "$geometry" "$work/black.png"
		set_background ffffff
		WAYLAND_DISPLAY=$wl grim -g "$geometry" "$work/white.png"
		set_background "$BACKGROUND"
		magick "$work/black.png" "$work/white.png" -alpha off \
			\( -clone 0,1 -compose difference -composite -negate \) \
			\( -clone 0,2 +swap -compose DivideDst -composite \) \
			-delete 0,1 +swap -compose CopyOpacity -composite \
			-define png:color-type=6 -define png:compression-level=9 "$1"
		;;
	gif)
		geometry=$(layer_geometry "$2") || die "$1: the $2 layer disappeared before the recording"
		record "$1" "$3" "$geometry"
		;;
	esac
}

for name in $selected; do
	printf 'screenshots: %s\n' "$name" >&2
	if [ "$name" = all-outputs ]; then
		create_output "$second_kind" "$SECOND"
		$window && size_windows
	fi
	fifo=$work/$name.stdin
	mkfifo "$fifo"
	(
		unset DISPLAY
		export WAYLAND_DISPLAY="$wl" HYPRLAND_INSTANCE_SIGNATURE="$sig"
		export DBUS_SESSION_BUS_ADDRESS="$bus" TOUCHCUE_SCREENSHOT_OUTPUT="$shot_output"
		exec "$example" "$name"
	) <"$fifo" >"$work/$name.out" 2>"$work/$name.err" &
	example_pid=$!
	# Holds the example's stdin open; closing it releases the prompts.
	exec 3>"$fifo"
	poll "$READY_LIMIT" ready_seen || die "$name: the example did not get ready; see $work/$name.err"
	namespace=$(sed -n 's/^ready //p' "$work/$name.out")
	if ! poll "$LAYER_LIMIT" has_layer "$namespace"; then
		# Captures failure diagnostics.
		hctl -j layers >"$work/layers.json" 2>&1 || true
		cp -- "$XDG_RUNTIME_DIR/hypr/$sig/hyprland.log" "$work/hyprland-instance.log" 2>/dev/null || true
		die "$name: no $namespace layer appeared; see $work/layers.json"
	fi
	if [ "$namespace" = touchcue-modal ]; then
		poll "$LAYER_LIMIT" has_layer touchcue || die "$name: no popup layer appeared"
	fi
	sleep "$SETTLE"
	image=$(image_of "$name")
	capture "$name" "$namespace" "$work/$image"
	exec 3>&-
	poll "$STOP_LIMIT" eval '! alive "$example_pid"' || stop "$example_pid"
	if wait "$example_pid"; then
		example_pid=
	else
		code=$?
		example_pid=
		die "$name: the example exited with status $code; see $work/$name.err"
	fi
	mv -- "$work/$image" "$out_dir/$image"
	printf 'screenshots: wrote %s\n' "$out_dir/$image" >&2
	if [ "$name" = all-outputs ]; then
		hctl output remove "$SECOND" >>"$work/hyprctl.log" 2>&1 ||
			die "removing output $SECOND failed; see $work/hyprctl.log"
		poll "$START_LIMIT" eval '! has_output "$SECOND"' || die "output $SECOND was not removed"
	fi
done
