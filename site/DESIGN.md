---
name: lets
description: The lets site as a warm GNOME desktop, where every section is a window that does the thing it describes.
colors:
  ink: "#241F31"
  ink-secondary: "#5E5C64"
  ink-tertiary: "#77767B"
  paper: "#FFFFFF"
  gutter: "#F7F6F3"
  well: "#F4F3EF"
  headerbar-top: "#F6F5F2"
  headerbar-bottom: "#E9E7E2"
  action-blue: "#1C71D8"
  action-blue-top: "#3584E4"
  action-blue-edge: "#134C92"
  key-blue-top: "#2474D8"
  key-blue-bottom: "#1A62BE"
  key-blue-border: "#0F3F7A"
  link: "#1A5FB4"
  done-green: "#26A269"
  key-done-top: "#23955C"
  key-done-bottom: "#1B7A4B"
  change-yellow: "#F6D32D"
  note-amber: "#E5A50A"
  error-red: "#C01C28"
  error-red-top: "#E01B24"
  error-red-edge: "#8A1219"
  key-red-top: "#D42833"
  key-red-bottom: "#B31924"
  terminal-bg: "#1E1B24"
  terminal-text: "#E9E6EE"
  terminal-prompt: "#8FF0A4"
  terminal-path: "#99C1F1"
  terminal-error: "#FF8C94"
  graphite-ink: "#EDEBF0"
  graphite-ink-secondary: "#B9B7BF"
  graphite-ink-tertiary: "#A09EA6"
  graphite-paper: "#242529"
  graphite-gutter: "#2B2C31"
  graphite-well: "#1D1E22"
  graphite-headerbar-top: "#393A40"
  graphite-headerbar-bottom: "#2E2F34"
  graphite-link: "#99C1F1"
  graphite-error: "#FF7B84"
  graphite-terminal-bg: "#141318"
  pistachio-base: "#C9D8B4"
  pistachio-chip-large: "#A3B98D"
  pistachio-chip-dark: "#7E9A6A"
  pistachio-chip-light: "#F1F4EA"
  apricot-base: "#F0C7A0"
  apricot-chip-large: "#DDA879"
  apricot-chip-dark: "#B7835A"
  apricot-chip-light: "#FBEBDD"
  lagoon-base: "#8FC4BE"
  lagoon-chip-large: "#72ACA6"
  lagoon-chip-dark: "#4F8B86"
  lagoon-chip-light: "#E3F1EF"
  graphite-base: "#3A3F45"
  graphite-chip-large: "#2F3338"
  graphite-chip-dark: "#4F5861"
  graphite-chip-light: "#7C858E"
typography:
  display:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "60px"
    fontWeight: 700
    lineHeight: 1.02
    letterSpacing: "-0.028em"
    fontVariation: "'wdth' 88"
  headline:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "34px"
    fontWeight: 700
    lineHeight: 1.12
    letterSpacing: "-0.02em"
    fontVariation: "'wdth' 92"
  title:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "19px"
    fontWeight: 700
    lineHeight: 1.3
  window-title:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 700
    lineHeight: 1.2
  body:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "17px"
    fontWeight: 400
    lineHeight: 1.6
  label:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 500
    lineHeight: 1.25
  numeral:
    fontFamily: "Ubuntu Sans, Ubuntu, system-ui, sans-serif"
    fontSize: "40px"
    fontWeight: 700
    lineHeight: "44px"
    fontFeature: "'tnum'"
    fontVariation: "'wdth' 80"
  command:
    fontFamily: "Ubuntu Sans Mono, Ubuntu Mono, ui-monospace, monospace"
    fontSize: "15px"
    fontWeight: 500
    lineHeight: 1.6
  output:
    fontFamily: "Ubuntu Sans Mono, Ubuntu Mono, ui-monospace, monospace"
    fontSize: "13.5px"
    fontWeight: 400
    lineHeight: 1.6
rounded:
  inline-code: "4px"
  key: "6px"
  pane: "8px"
  window: "10px"
  popover: "12px"
  notification: "14px"
  switch: "16px"
  round: "50%"
spacing:
  hairline: "1px"
  tight: "6px"
  control: "10px"
  paragraph: "14px"
  window-pad: "18px 20px 20px"
  window-stack: "28px"
  section: "56px"
  readme-width: "604px"
  readme-gutter: "46px"
  window-overlap: "-44px"
components:
  window:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.window}"
  header-bar:
    backgroundColor: "{colors.headerbar-bottom}"
    textColor: "{colors.ink}"
    typography: "{typography.window-title}"
    height: "42px"
    padding: "0 8px 0 14px"
  window-control:
    size: "24px"
    rounded: "{rounded.round}"
  key:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.key}"
    padding: "8px 14px"
  key-blue:
    backgroundColor: "{colors.key-blue-bottom}"
    textColor: "{colors.paper}"
    rounded: "{rounded.key}"
    padding: "8px 14px"
  key-blue-done:
    backgroundColor: "{colors.key-done-bottom}"
    textColor: "{colors.paper}"
    rounded: "{rounded.key}"
  key-red:
    backgroundColor: "{colors.key-red-bottom}"
    textColor: "{colors.paper}"
    rounded: "{rounded.key}"
  key-copy:
    backgroundColor: "{colors.key-blue-bottom}"
    textColor: "{colors.paper}"
    rounded: "{rounded.pane}"
    height: "56px"
    width: "100%"
  well:
    backgroundColor: "{colors.well}"
    textColor: "{colors.ink}"
    rounded: "{rounded.window}"
  command-link:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.key}"
    padding: "6px 10px"
  top-panel:
    backgroundColor: "{colors.headerbar-bottom}"
    textColor: "{colors.ink}"
    height: "34px"
  taskbar:
    backgroundColor: "{colors.headerbar-bottom}"
    textColor: "{colors.ink}"
    height: "40px"
  desktop-icon-label:
    textColor: "{colors.ink}"
    typography: "{typography.label}"
    rounded: "5px"
  desktop-icon-label-selected:
    backgroundColor: "{colors.action-blue}"
    textColor: "{colors.paper}"
  terminal:
    backgroundColor: "{colors.terminal-bg}"
    textColor: "{colors.terminal-text}"
    typography: "{typography.output}"
    height: "340px"
  compare-tab:
    textColor: "{colors.ink-secondary}"
    rounded: "8px 8px 0 0"
    padding: "9px 14px"
  compare-tab-selected:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
  compare-pane:
    backgroundColor: "{colors.paper}"
    rounded: "{rounded.pane}"
  notification:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.notification}"
    width: "380px"
  appearance-popover:
    backgroundColor: "{colors.paper}"
    textColor: "{colors.ink}"
    rounded: "{rounded.popover}"
    width: "292px"
  switch:
    backgroundColor: "{colors.key-blue-top}"
    rounded: "{rounded.switch}"
    size: "58px"
    height: "32px"
---

# Design System: lets

## Overview

**Creative North Star: "The Lived-in Workstation"**

The page is a Linux desktop in the GNOME style, seen on an ordinary afternoon. A terrazzo wallpaper fills the viewport behind a top panel, a column of desktop icons, a taskbar, and a stack of real-feeling windows. The README is the tall Text Editor window running down the left. Every other section is an application window that performs its claim instead of describing it. The switch window toggles the transcript. The terminal runs recorded commands. The Compare window lines up the turns each call replaces. The spreadsheet holds the trial numbers. The hook is a dialog, the old habits sit in the Trash, the verbs live in a file manager, and llms.txt opens in a plain text window. The docs page is the same desktop with a Help viewer open.

The material is tactile and warm. Header bars carry a soft vertical gradient and a one-pixel inner highlight. Keys have a hard two-pixel bottom lip and press down one pixel. Wells are sunken with an inset shadow. Windows float on a two-layer shadow that deepens when they take focus. Density is that of a real desktop at 1440 wide: README body text at 17px, command output at 12.5 to 13.5px monospace, window chrome at 13 to 14.5px.

Every piece of lets output on the page was recorded from a real run of lets 0.0.1. Nothing in an output pane is written by hand.

**Key Characteristics:**
- One world: a desktop with a panel, icons, a taskbar and windows, carried through both pages.
- A window's content does its section's job: a toggle, a terminal, a diff, a sheet, a dialog, a file list.
- A tactile surface language: gradient header bars, lipped keys, sunken wells, focus-deepening shadows.
- Four wallpapers switched from the panel, one of them dark (Graphite) that re-tones every surface.
- Numbered lines everywhere text is shown, as in the tool's own output.

## Colors

The palette is GNOME's own: warm off-whites and a violet-black ink over a coloured terrazzo, with Adwaita blue for actions and a yellow highlighter for changed lines.

### Primary
- **Adwaita Action Blue** (action-blue, with action-blue-top for highlights and action-blue-edge for the key lip): selected desktop-icon labels, the current page in the Help viewer's tree, focus outlines (action-blue-top, 2px), the "on" switch, and the Compare bands (action-blue-top mixed into paper: 12% at rest, 24% on hover, with a 48% stroke). The primary keys (Copy install, and the taskbar copy key) use their own gradient from key-blue-top to key-blue-bottom, with a key-blue-border edge.
- **Link Blue** (link): text links, output header lines (`── path (1-13 of 13)`), and the docs page's placeholder variables.

### Secondary
- **Highlighter Yellow** (change-yellow): changed lines (`~` lines) and search hits. It is applied at 42% alpha on light surfaces and 28% on Graphite. At full strength it fills text selection and the active anatomy callout numeral.
- **Done Green** (done-green; key-done-top to key-done-bottom on a key): the ▸ glyph on README command links, the "copied" state of the blue key, and the good values in the spreadsheet.

### Tertiary
- **Revert Red** (error-red, with error-red-top for tints and error-red-edge for the key lip): `REVERTED`, `ERROR_CODE=` lines, the parse-error line's tint (error-red-top at 14 to 16%), strikethroughs in the Trash, and the Empty Trash key (key-red-top to key-red-bottom).
- **Note Amber** (note-amber): the comment corner on the spreadsheet's note row and the border of callout notes in the docs.

### Neutral
- **Violet Ink** (ink): all primary text. It is violet-black, never pure black.
- **Slate Ink** (ink-secondary): secondary text, captions and the status bar.
- **Pebble Ink** (ink-tertiary): line numbers, prompts and turn numbers.
- **Paper** (paper): window bodies.
- **Gutter** (gutter): the README line-number gutter, pane headers, status bars and tab strips.
- **Well** (well): sunken inputs, inline code, the install steps, the ask-and-run control. Its border is 10% black.
- **Header Bar** (headerbar-top to headerbar-bottom): header bars, the top panel and the taskbar, with an 18%-black bottom line.
- **Hairline** (8% black): dividers inside windows. **Window Edge** (18% black): window and pane borders.

### Terminal
The terminal is always dark, whatever the wallpaper. It uses terminal-bg (graphite-terminal-bg under Graphite) with terminal-text, a terminal-prompt green prompt, a terminal-path blue path and header lines, terminal-error for errors, and yellow at 22% for changed lines.

### Graphite
Graphite is the dark theme, not only a wallpaper. It swaps ink, paper, gutter, well, header bar, link and error for their graphite-* tokens. Hairlines become 8% white and edges become 60% black. Keys, command links and the focused header bar get dark gradients (#4A4B52 to #3B3C42, and #45464D to #393A40). The two coloured keys keep their light-theme gradients.

**The Violet Ink Rule.** Text is violet ink or one of its two lighter steps. Use no pure black and no grey that is not one of these three.

**The Highlighter Rule.** Yellow means "this line changed" or "this matched". It never decorates.

## Typography

**Display Font:** Ubuntu Sans (with Ubuntu, system-ui, sans-serif), a variable font on the wdth and wght axes, loaded from Google Fonts.
**Body Font:** Ubuntu Sans.
**Label/Mono Font:** Ubuntu Sans Mono (with Ubuntu Mono, ui-monospace, monospace), weights 400 to 700, with ligatures off.

**Character:** This is the desktop's own system face, so the page reads as the operating system talking. Headings tighten the width axis (wdth 88 to 92) for a compact, confident headline without changing family.

### Hierarchy
- **Display** (display): the README's one h1. It is balanced text, and 40px below 1100px wide.
- **Headline** (headline): section h2s, with 56px of space above. The docs h1 is 44px/1.1 and docs h2s are 27px/1.2 above a hairline rule.
- **Title** (title): h3s in the README and docs. The Compare panel title is 20px/1.3; the hook dialog heading is 18px/1.3.
- **Window title** (window-title): centred in every header bar. A regular-weight, secondary-ink suffix carries the path.
- **Body** (body): the README paragraphs, capped at 34em. Docs paragraphs are 17px/1.7, capped at 38em. The hero subline is 19px/1.5 in secondary ink.
- **Label** (label): desktop-icon labels, 13px, on a 72% wallpaper-tinted chip. The panel text is 500 14.5px, and status bars and captions are 12.5 to 13px.
- **Numeral** (numeral): the switch window's rolling turn counter, with tabular figures in the narrowest width.
- **Command** (command): commands a reader copies (install steps, the docs synopsis at 15px/1.75).
- **Output** (output): the lets output blocks. Compare panes use 12.5px, the terminal 13px/1.55, and the anatomy drawing 12px.

**The Numbered Line Rule.** Text shown as a file carries line numbers in pebble ink: the README gutter numbers each block, and the text windows and outputs number each line. It is the tool's own output format, worn as the page's texture.

## Layout

The desktop frame is fixed. A 34px top panel runs along the top (44px below 1100px), a 100px icon column sits on the left, and a 40px taskbar runs along the bottom. The page content sits in `.desk` with 120px of left padding to clear the icons.

The README is one continuous 604px Text Editor window behind every section. It has a 46px line-number gutter, 34px of left text padding and 58px of right padding. Each section is a two-column grid: the README text on the left (604px), and the section's windows in the remaining width, overlapping the README's right edge by 44px so they read as windows placed on top of it. At 1100px and wider, the first window in a standard row sticks 20px below the panel while its README text scrolls past.

Three layout variants keep the rows from repeating:
- **Over**: the switch window spans both columns, up to 880px wide, laid over the README, with its caption continuing in the README below it.
- **Wide**: the Compare window takes the full width of the desk.
- **Right**: the file manager is pushed 300px in from the left, up to 940px wide, so it sits across the README's right side.

**Measured rhythm:** 56px above each h2 and before each side window; 28px between stacked windows; 14px between paragraphs; 18px 20px 20px inside a padded window body. At 1440 wide, no README stretch below the first screen is empty for more than about 160px.

**Below 1100px** the desktop becomes a home screen. Icons form a horizontally scrolling row under the panel. The taskbar, the background README window and most window controls are hidden. The README breaks into one card per section, and every side window stacks full-width 16px below its text. Below 900px, the Compare window stacks its two panes and drops the bands. Side gutters are 10px at phone width, with no horizontal scroll at 375px.

The docs page keeps the panel and icons and opens one Help window, up to 1260px wide. Its sticky header bar sits over a 300px sidebar (a quick-start card and the page tree) and an 880px article.

## Elevation & Depth

Depth is literal: windows are physical sheets stacked on the wallpaper. Two shadow tokens carry it, a resting one and a focused one. Clicking a window raises it to the focused shadow and z-index 20. The README is the exception: it stays at the bottom of the stack. Inside a window, depth comes from sunken wells and raised keys rather than more shadows.

### Shadow Vocabulary
- **Window at rest** (`box-shadow: 0 1px 2px rgba(0,0,0,.12), 0 12px 32px -8px rgba(0,0,0,.28)`): every window, the docs footer, and the README cards on mobile.
- **Window focused** (`box-shadow: 0 1px 2px rgba(0,0,0,.14), 0 22px 48px -10px rgba(0,0,0,.38)`): the active window.
- **Graphite shadows** (`0 1px 2px rgba(0,0,0,.4), 0 14px 36px -8px rgba(0,0,0,.6)` at rest; `0 1px 2px rgba(0,0,0,.45), 0 24px 52px -10px rgba(0,0,0,.75)` focused).
- **Floating surface** (`0 16px 40px -10px rgba(0,0,0,.4), 0 1px 2px rgba(0,0,0,.15)`): the copy notification. The appearance popover is slightly deeper (`0 18px 44px -12px rgba(0,0,0,.45)`).
- **Key lip** (`inset 0 1px 0 rgba(255,255,255,.9), 0 2px 0 rgba(0,0,0,.22), 0 3px 6px -3px rgba(0,0,0,.25)`): the neutral key. Coloured keys use a 2px lip in their edge colour.
- **Sunken well** (`inset 0 2px 4px rgba(0,0,0,.07)`): the install steps, the switch control well, and the docs synopsis.
- **Header highlight** (`inset 0 1px 0 rgba(255,255,255,.7)`; .08 on Graphite): the top edge of every header bar.

**The Lip Rule.** A pressable thing has a hard 2px bottom lip and moves down 1px when pressed, keeping a 1px lip. A thing without a lip is not pressable.

## Shapes

Corners are gently rounded and nest. A window is 10px, and its header bar and bottom bars are 9px, so they sit inside the 1px border. Panes and pane-level blocks are 8px, keys 6px, inline code 4px. Floating surfaces are rounder: the popover is 12px and the notification 14px. Circles are for counters and window controls: step numbers, turn numbers, callout numerals and the 24px close, minimise and maximise buttons. Dashed borders mean "not really here": the phantom turns in the anatomy drawing and the note left behind by a minimised or closed window. The wallpaper's chips are irregular four- to seven-sided polygons, generated in three seamless tiles.

## Components

### Windows
- **Character:** a GTK4 window with an Adwaita header bar.
- **Shape:** a 10px radius, a 1px window-edge border, and the resting shadow.
- **Header bar:** at least 42px tall, with an optional leading icon or key, a centred bold title, and circular controls on the right (24px, ink at 8%, 16% on hover, 24% when pressed). The focused bar is lighter.
- **Behaviour:** click to focus. Minimise flies the window into its desktop icon; restore flies it back out. Maximise fills the desk between the panel, the icons and the taskbar. Close leaves a dashed note with a Reopen button. The taskbar lists open windows, marks the pressed one, and italicises minimised ones.
- **Status bar:** 12.5px secondary ink on gutter, under a hairline.

### Keys (buttons)
- **Shape:** a 6px radius (8px, and 56px tall, for the full-width Copy install key).
- **Neutral:** a white-to-#EEEDE9 gradient, a 22%-black border, and the key lip.
- **Primary:** the blue key gradient with a 1px dark top shadow on the label. After a copy it turns green and draws a tick.
- **Destructive:** the red key, used once, for Empty Trash.
- **Command link:** in the README's try-list, each command is a small neutral key in mono, prefixed with a green ▸. Pressing one types it into the terminal.

### Top panel, desktop icons, taskbar
- **Panel:** the header-bar gradient with the lets logo (a terminal glyph), section links, the appearance button and a quiet link to the docs. Hover is ink at 9%.
- **Icons:** 48px symbolic-colour glyphs, with labels on a wallpaper-tinted chip. Hovering lifts the glyph 2px and turns the label chip blue. Each icon opens or focuses its window, and the Trash sits pinned to the bottom of the column.

### Install block
A sunken well of numbered steps (26px circle numerals, 15px mono commands, hairlines between the steps), followed by the full-width blue Copy key and a secondary line of fine print.

### Terminal
The dark body described under Colors, 340px of scrolling output with a live prompt line below it. The caret is an 8px block that blinks on a 1.1s step while the prompt is focused, and the focused line gets a 2px inset blue ring. On load it prints one recorded command at the top of its scroll. The README's command links and typed commands replay recorded output only.

### Compare (signature component)
A wide window in the style of Meld, the GNOME diff viewer.
- **Tabs:** a tab strip on gutter with one tab per pair. The selected tab joins the paper panel below it.
- **Panel:** a 20px title, a one-line claim, then three columns (5fr, 72px, 7fr).
- **Left pane, "Without lets":** a header with the turn count, and one blue-tinted row per turn: a circle number, the command in mono, and a note on what the turn was for.
- **Right pane, "With lets":** a header reading "1 call", the bold command, then the recorded output. The output is split into blue-tinted chunks, one per turn it makes unnecessary.
- **Bands:** curved SVG ribbons joining each turn row's top and bottom edges to its chunk's. They redraw on resize. Hovering a row, a band or a chunk lights all three at 24%.
- **Keyboard:** the tablist uses roving tabindex with the arrow keys, Home and End.
- **Fallbacks:** with JavaScript off, all seven pairs render stacked. Below 900px the panes stack and the bands hide.

### Switch window
A sunken control well holding two rolling numerals (7 turns and 2 turns), a 58×32 GNOME switch with a 26px knob, and a transcript. The transcript shows either the stock turns or the lets turns, never both at once.

### Spreadsheet, dialog, Trash, file manager, text window
- **Spreadsheet:** a formula bar, a grid with a gutter-coloured header and row numbers, arrow-key cell navigation, and sheet tabs along the bottom. The note row is tinted yellow with an amber comment corner.
- **Hook dialog:** a 52px shield icon, a heading, the blocked command, the replacement in a well, and keys on the right. It steps through the blocked examples, and a counter sits on the left.
- **Trash:** a floating window of three "habits", each a file card showing the struck-through old command and the `run:` replacement. Empty Trash crumples them away; Put back restores them.
- **File manager:** a path bar of pill segments and a list table with mono verb names. Rows are tinted blue on hover and focus.
- **Text window:** a line-number gutter beside plain text, with document links below.

### Notification and appearance popover
- **Notification:** a GNOME banner that drops from under the panel, centred and 380px wide, with an icon, a bold line, a secondary line and a round dismiss button. It hides after 3.6s.
- **Appearance popover:** 292px, opening from the panel. It offers a 2×2 grid of wallpaper swatches, each a live terrazzo preview; the checked one gets a 2px blue outline offset 2px.

### Help viewer (docs page)
- **Frame:** a sticky header bar with a "View as markdown" key that links to the markdown source.
- **Sidebar:** a quick-start card (numbered commands in a well, plus a Copy key) and a page tree. The current page is a blue pill, and the current section on it is tinted blue.
- **Article:** synopsis wells with italic blue placeholders, and "try" blocks (a command with a Copy command key, above its recorded output).
- **Blocks:** figures captioned with an exit-code pill (red, or green for exit 0), option tables that become cards on mobile, and amber notes.

## Motion

Every transition uses one ease-out curve, cubic-bezier(0.16, 1, 0.3, 1). The one exception is things leaving toward a target, which use an ease-in.

- **Wallpaper change:** the four registered wallpaper colours cross-fade over 0.45s. This runs under reduced motion too, because it is a colour change, not movement.
- **Entrance:** once, on a desktop-width first load near the top of the page, the README and the anatomy window grow out of their icons (400ms from scale 0.18, the anatomy window 80ms later).
- **Windows:**
  - focus shadow: 250ms;
  - minimise into the icon: 280ms, cubic-bezier(0.7, 0, 0.84, 0);
  - restore out of the icon: 400ms;
  - maximise (the window moves and resizes from its old box): 300ms;
  - close: 160ms ease-in to scale 0.96;
  - reopen: 220ms.
- **Keys:** the press takes 100ms. The copy tick draws in 320ms.
- **Notification and popover:** the notification slides in over 350ms. The popover pops from scale 0.96 in 180ms.
- **Switch:**
  - the knob slides in 220ms and the numerals roll in 450ms;
  - the stock turns collapse into the lets turns (380ms, 28ms stagger) while the new turns rise 8px (280ms, 90ms stagger);
  - switching off reverses this (340ms).
- **Compare bands:** on a tab change the bands grow out from the left pane (380ms, starting 60ms in, 70ms between bands).
- **Hook dialog:** each example fades up 4px in 200ms.
- **Trash:** Empty Trash crumples each card over 460ms, 90ms apart. Put back scales the cards in over 260ms, 60ms apart.

**Reduced motion:**
- Every scripted animation is skipped. The one exception: minimise and restore become a 150ms opacity fade.
- CSS animations are cut to 0.01ms.
- The icon lift, the key press, the switch knob, the counter roll and the anatomy lift do not move.
- The notification appears in place.

**The One Curve Rule.** Motion eases out on cubic-bezier(0.16, 1, 0.3, 1). Only an exit toward a destination (minimise, crumple) eases in.

## Wallpapers

The wallpaper is a fixed layer: a base colour plus three masked speckle tiles, generated as seamless SVG masks.
- **Large tile:** 460px, 34 chips 2.2 to 6.5px across, in the large-chip colour.
- **Middle tile:** 390px, 70 chips, in the dark-chip colour at 85% opacity.
- **Small tile:** 330px, 60 chips, in the light-chip colour.

The four sets:
- **Graphite** (the default): graphite-base with charcoal and slate chips and a pewter fleck. It also switches every surface to the dark tokens.
- **Pistachio:** pistachio-base with sage and moss chips and a cream fleck.
- **Apricot:** apricot-base with caramel and toffee chips and a peach-cream fleck.
- **Lagoon:** lagoon-base with teal chips and a sea-foam fleck.

The choice is saved in the browser and applied before the first paint. Icon labels sit on a chip mixed from the current base colour, so they stay legible on every wallpaper.

## Do's and Don'ts

### Do:
- **Do** put a new section in a window whose app does the section's job (a sheet for numbers, a dialog for a refusal), with a real header bar, controls and a taskbar entry.
- **Do** use recorded lets output verbatim in any output pane, marking changed lines yellow and reverts and errors red.
- **Do** give every pressable control the lip and the 1px press, and give every focusable one the 2px action-blue-top ring.
- **Do** vary the row layout (over, wide, right) so two adjacent sections never share the same text-left, window-right shape.
- **Do** keep every surface on the tokens, so Graphite re-tones it with no extra rules.

### Don't:
- **Don't** use pure black text, or a grey outside the three ink steps.
- **Don't** use highlighter yellow as decoration. It marks changed or matched text only.
- **Don't** add a second easing curve, or animate anything that reduced motion does not also skip.
- **Don't** make the terminal light. It stays dark on every wallpaper.
- **Don't** leave a README stretch below the first screen empty for more than about 160px at 1440 wide.
