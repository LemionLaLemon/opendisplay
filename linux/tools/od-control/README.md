# od-control (companion app)

A small PySide6 app for `od-sender`: start/stop it, switch between extending
the desktop and using the receiver as a drawing tablet, pick the transport
and receiver, and tune the pen and encoder without editing the config file.

```sh
python3 tools/od-control/od_control.py
```

Needs PySide6 (`pacman -S pyside6`), `hyprctl` for the monitor list on
Hyprland, and `od-sender` on `PATH`.

Changes to the **pen action** and **active area** are applied live over the
sender's `--control-socket`, so the display is not rebuilt. Everything else
(transport, mode, monitor, CRF, bitrate, preset) is used the next time Start
is pressed; Stop sends SIGTERM so a virtual display is removed cleanly.

The window keeps its settings in `~/.config/opendisplay/gui.conf`.
