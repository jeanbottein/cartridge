•cartridge themes
=================

Each folder here is a theme, named after the folder: pick it in
SETTINGS > DISPLAY > Theme. To make your own, copy "Saturday Morning",
rename the copy and edit its theme.json. Changes show when you open
SETTINGS again.

A deleted folder stays deleted: this folder is only filled in when it
doesn't exist (delete it to get Saturday Morning back).

theme.json
----------
Every key is optional: what a theme leaves out comes from Midnight.

  font          "barlow", "pixel", or a .ttf / .otf file in the theme's folder
  font_size     body text size of that file, in interface pixels (default 18)
  radius        corner radius of cards and buttons, 0 for square corners
  glow          true: a soft halo around the focused card
  focus_width   outline width of the focused card and of dialogs
  card_outline  true: outline every card, not only the focused one

  colors        "#rrggbb", or "#rrggbbaa" with opacity:
    bg_top, bg_bottom   background gradient, top to bottom
    surface             cards and panels
    surface_hi          the focused card or row
    border              outlines and separators
    shadow              behind dialogs (transparent: no shadow, no dimming)
    text, muted, disabled
    accent              focus, confirm, success
    secondary           titles, warnings, going back
    info                information, Steam, folders
    danger              errors and destructive actions

  tabs          a color per tab, replacing accent while that tab is open:
                library, mods, saves, updates, network, settings

Only share fonts whose license allows it, with that license next to them.
