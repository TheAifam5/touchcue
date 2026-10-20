#!/bin/sh
# Renders the hero animation of the documentation landing page without a
# display and encodes it as docs/public/screenshots/hero.webp, an endlessly
# looping animated WebP with alpha. Frames are written to target/hero.
#
# Usage: scripts/hero.sh
#
# Needs the example built with `cargo build -p touchcue-ui --example
# screenshots`, and ffmpeg with the libwebp_anim encoder or ImageMagick
# (magick) with WebP support.
set -eu

# Frames per second of the animation, and WebP quality from 0 to 100.
FPS=30
QUALITY=90

die() {
	printf 'hero: %s\n' "$*" >&2
	exit 1
}

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
example=${CARGO_TARGET_DIR:-$repo/target}/debug/examples/screenshots
frames=$repo/target/hero
out=$repo/docs/public/screenshots/hero.webp

[ -x "$example" ] || die "$example is missing; run: cargo build -p touchcue-ui --example screenshots"
rm -rf -- "$frames"
"$example" --frames hero "$frames" --fps "$FPS" || die "the example failed to render the frames"

if command -v ffmpeg >/dev/null 2>&1 && ffmpeg -hide_banner -encoders 2>/dev/null | grep -q ' libwebp_anim '; then
	ffmpeg -hide_banner -loglevel error -y -framerate "$FPS" -i "$frames/frame-%04d.png" \
		-c:v libwebp_anim -pix_fmt yuva420p -quality "$QUALITY" -loop 0 "$out" ||
		die "ffmpeg failed to encode $out"
elif command -v magick >/dev/null 2>&1 && magick -list format 2>/dev/null | grep -Eq '^ *WEBP\*? +WEBP +rw'; then
	magick -delay "1x$FPS" -dispose Background "$frames"/frame-*.png \
		-loop 0 -quality "$QUALITY" "$out" ||
		die "magick failed to encode $out"
else
	die "neither ffmpeg with libwebp_anim nor magick with WebP support is installed"
fi
printf 'hero: wrote %s\n' "$out"
