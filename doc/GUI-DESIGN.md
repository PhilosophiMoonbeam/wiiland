# WiiLand Control Center

WiiLand is a native, offline control center for Wii input on Linux, implemented
in egui/eframe with Wayland and X11 backends. It should feel like a calm precision
instrument: connection and service state first, deliberate tuning second,
advanced diagnostics only when needed. The logo informs the palette, not the
layout; a decorative welcome must not displace the next useful action.

## Visual language

- Use the shared palette and helpers in `crates/wiiland-config/src/theme.rs`.
  Pearl combines a warm neutral canvas (`#F2F1EA`), nearly-white opaque cards,
  evergreen text, and rich teal actions. Dusk uses graphite-blue surfaces with
  distinct canvas, card, and inset levels; sea-glass is an accent, not a tint
  applied to everything.
- Keep glass, reflections, and illustration detail in the emblem. Cards are
  flat opaque surfaces with fine boundaries. Do not add gradients, external
  fonts, remote imagery, or decorative dashboard metrics.
- Typography carries hierarchy: 28px page titles, 19px section headings, 14px
  body and controls, 13px supporting notes, and 12px metadata/monospace output.
  Use the bundled egui fonts. Keep prose short enough to expose the task.
- Shared cards use 16px insets and 8px corners. Controls and menus use 6px
  corners, small status labels 4px, and windows 10px. The base vertical gap is
  8px and interaction height is at least 32px. Prefer grouping over repeated
  large gaps; compact screens must not pay for desktop decoration.
- Reserve solid accent fills for the primary action. Selections use the mist
  surface and accent text. Hover adds an accent boundary; keyboard focus uses
  a stronger 2px outline. Solid primary buttons retain a contrasting inset
  focus indicator rather than hiding egui's state behind a custom fill.
- Disabled primary actions lose the accent fill and use a neutral, faded
  treatment; they must not look ready to run. Ordinary controls also fade when
  disabled. Neutral badges use muted text, not success green. Warning badges
  use amber and explanatory text; color alone never communicates service state.
- Keep form labels visible and aligned. Attach each field's label to its
  accessibility response; never rely on placeholder text alone.

The palette's sRGB relative-luminance contrast ratios provide a maintenance
baseline, not a substitute for inspecting the rendered interface:

| Text/background | Pearl | Dusk |
| --- | ---: | ---: |
| Body ink/card | 11.66:1 | 12.29:1 |
| Supporting copy/card | 5.69:1 | 7.22:1 |
| Supporting copy/mist | 4.85:1 | 5.62:1 |
| Primary label/accent | 6.56:1 | 6.78:1 |
| Warning label/mist | 5.50:1 | 6.83:1 |
| Error label/card | 6.69:1 | 7.26:1 |

These opaque text combinations exceed 4.5:1, including small supporting copy.
Accent focus boundaries contrast with cards at 6.67:1 in Pearl and 8.02:1 in
Dusk. Subtle card separators are decorative; do not use them as the sole
indicator of selection or focus. Disabled controls are intentionally faded and
are not represented by these enabled-text ratios.

## Workflows

Overview is the connection workspace: a compact introduction establishes the
Connect / Configure / Test sequence, while service state and device discovery
receive the useful space. Configuration file & advanced settings keeps the
configuration location and executable details in a secondary disclosure rather
than a second welcome panel.

Configure separates profile/pointer tuning, motion aiming, button bindings,
and ordered device rules. Pointer response and motion Response are everyday
adjustments. Advanced IR mapping & screen calibration and Saved sensor
calibration are disclosures; do not let numeric calibration triples dominate
the default form. Desktop buttons keeps the physical button names alongside
their actions.

Controller rules run top to bottom, with the last matching rule winning.
Move earlier lowers priority, Move later raises it, and the boundary buttons
are disabled. Keep this ordering visible; a rearrange action changes behavior,
not just presentation.

Test & calibrate presents daemon input capture and flat-surface calibration
first. Advanced capture options contains direct-hardware mode; Saved-file
diagnostics & reference holds the offline checks. Disclosures reduce initial
noise without removing capabilities or hiding active capture cancellation.

Save controls remain outside the scrolling page and visible across all tabs.
Unsaved edits remain visible across navigation; reload and close use a modal
discard confirmation. Ctrl+S validates and saves without restarting. Custom
files cannot restart the service. A visual redesign must preserve these
transaction boundaries rather than implying that editing applies live.

The activity drawer opens for diagnostic commands and captures, and can be
resized or hidden. A running capture always has a Stop capture action in the
status strip, even after navigating away. Service status reflects a status
query rather than the success of a preceding service command. Cancelled captures
release their ownership without applying partial calibration values.

The minimum viewport is 760 × 600; the desktop reference is 1180 × 780.
Below 1000 pixels wide, navigation moves from the 192px numbered sidebar into
the header. Overview cards stack when content width falls below 620px. The save area uses two rows.
Activity output wraps vertically rather than forcing a wide page. Save
controls and the global Stop capture action remain reachable when the log is
open. Pearl and Dusk share layout and hierarchy; appearance follows the system
by default and can also be selected explicitly.

## Assets and verification

`res/io.github.philosophimoonbeam.wiiland.svg` is the transparent emblem extracted
from `res/wiiland-logo.svg`. `res/wiiland-icon.png` is its 512 × 512 raster export,
embedded in the binary for the native window icon.
No runtime asset downloads or font dependencies are needed.

For maintenance, inspect the actual native window at both reference sizes in
both palettes, including keyboard focus, disabled primary actions, long paths,
and expanded activity output. Exercise navigation, editing, discard
confirmation, saving, and capture cancellation without a controller attached;
hardware absence must produce useful status, not a broken layout.
`cargo test --locked -p wiiland-config` is the existing model, asynchronous-task,
and egui interaction test entry point. Automated coverage complements, but does
not replace, this native visual and interaction review.
