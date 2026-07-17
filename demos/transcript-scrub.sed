# Normalize the per-run wall-clock noise out of a demo transcript, so a
# base-vs-PR diff (diff.html on the media repo) highlights real UI changes
# rather than the clock. Human-read only, never a merge gate — see
# docs/plans/screencasts.md "Change detection" for why committed/gated
# transcripts were not viable here.
#
# Apply with:  asciinema convert -f txt <cast> - | sed -E -f demos/transcript-scrub.sed

# makepkg build stamps: "(Fri Jul 17 17:56:24 2026)"
s/\((Mon|Tue|Wed|Thu|Fri|Sat|Sun) [A-Za-z]{3} [ 0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2} [0-9]{4}\)/(DATE)/g
# AUR "last modified" age cell: "(35m ago)"
s/\([0-9]+[smhd] ago\)/(AGE ago)/g
# trailing whitespace the vt100 grid leaves on padded lines
s/[[:space:]]+$//
