#!/usr/bin/env python3
"""Companion app for the OpenDisplay Linux sender.

Starts/stops `od-sender`, picks the mode (extend vs drawing tablet), transport
and quality, and adjusts the pen area live over the sender's control socket.

Run: python3 od_control.py
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys

from PySide6.QtCore import QProcess, QSettings, Qt, QThread, QTimer, Signal
from PySide6.QtGui import QFont
from PySide6.QtWidgets import (
    QApplication,
    QComboBox,
    QDoubleSpinBox,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QMainWindow,
    QPlainTextEdit,
    QPushButton,
    QRadioButton,
    QSizePolicy,
    QSlider,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

SOCKET = os.path.join(
    os.environ.get("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}"),
    "opendisplay-gui.sock",
)
SENDER = shutil.which("od-sender") or os.path.expanduser("~/.local/bin/od-sender")


def run_json(args: list[str], timeout: float = 5.0):
    try:
        out = subprocess.run(
            [SENDER, *args], capture_output=True, text=True, timeout=timeout
        )
        return json.loads(out.stdout or "[]")
    except Exception:
        return []


class ReceiverList(QThread):
    done = Signal(list)

    def __init__(self, transport: str):
        super().__init__()
        self.transport = transport

    def run(self):
        if self.transport == "usb":
            devs = run_json(["list", "--usb", "--json"], 3)
            self.done.emit(
                [
                    {
                        "name": (d.get("udid") or "?")[:18],
                        "addr": "",
                        "udid": d.get("udid"),
                        "kind": "usb",
                    }
                    for d in devs
                ]
            )
        else:
            recv = run_json(["list", "--json", "--timeout", "2"], 5)
            self.done.emit(
                [
                    {
                        "name": r.get("instance") or r.get("name") or "?",
                        "addr": r.get("addr") or "",
                        "kind": "wifi",
                    }
                    for r in recv
                ]
            )


class Control:
    """One-shot command to a running sender's control socket."""

    @staticmethod
    def send(line: str) -> str | None:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0)
                s.connect(SOCKET)
                s.sendall((line + "\n").encode())
                return s.recv(4096).decode().strip()
        except OSError:
            return None


class Window(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("OpenDisplay")
        self.setMinimumWidth(620)
        self.settings = QSettings("opendisplay", "gui")
        self.proc: QProcess | None = None
        self.loading = True
        self.receivers: list[dict] = []
        self.area_timer = QTimer(self)
        self.area_timer.setSingleShot(True)
        self.area_timer.setInterval(120)
        self.area_timer.timeout.connect(self.apply_area)

        root = QWidget()
        outer = QVBoxLayout(root)
        outer.setSpacing(6)

        def compact(group):
            group.setSizePolicy(QSizePolicy.Preferred, QSizePolicy.Maximum)
            return group

        # --- receiver -----------------------------------------------------
        rx = compact(QGroupBox("Receiver"))
        rl = QFormLayout(rx)
        self.transport = QComboBox()
        self.transport.addItems(["Auto (USB, else Wi-Fi)", "USB only", "Wi-Fi only"])
        self.transport.currentIndexChanged.connect(self.refresh_receivers)
        self.receiver = QComboBox()
        self.refresh_btn = QPushButton("Refresh")
        self.refresh_btn.clicked.connect(self.refresh_receivers)
        row = QHBoxLayout()
        row.addWidget(self.receiver, 1)
        row.addWidget(self.refresh_btn)
        rl.addRow("Transport", self.transport)
        rl.addRow("Device", row)
        outer.addWidget(rx)

        # --- display mode --------------------------------------------------
        mode = compact(QGroupBox("Mode"))
        ml = QVBoxLayout(mode)
        self.mode_extend = QRadioButton("Extend: a new virtual display on the iPad")
        self.mode_tablet = QRadioButton(
            "Tablet: pen drives an existing monitor, iPad is the surface"
        )
        self.mode_extend.setChecked(True)
        self.mode_extend.toggled.connect(self.sync_mode)
        ml.addWidget(self.mode_extend)
        ml.addWidget(self.mode_tablet)
        mrow = QHBoxLayout()
        mrow.addWidget(QLabel("Monitor"))
        self.monitor = QComboBox()
        self.monitor.setEnabled(False)
        mrow.addWidget(self.monitor, 1)
        ml.addLayout(mrow)
        outer.addWidget(mode)

        # --- pen -----------------------------------------------------------
        pen = compact(QGroupBox("Apple Pencil"))
        pl = QFormLayout(pen)
        self.motion = QComboBox()
        self.motion.addItems(
            [
                "absolute: cursor follows the pen position",
                "relative: stays put, moves with the pen (touchpad)",
            ]
        )
        self.motion.currentIndexChanged.connect(self.on_motion)
        pl.addRow("Motion", self.motion)

        self.sensitivity = QDoubleSpinBox()
        self.sensitivity.setRange(0.05, 5.0)
        self.sensitivity.setSingleStep(0.1)
        self.sensitivity.setDecimals(2)
        self.sensitivity.setValue(1.0)
        self.sensitivity.setSuffix(" screens per full sweep")
        self.sensitivity.valueChanged.connect(self.on_sensitivity)
        pl.addRow("Sensitivity", self.sensitivity)

        self.pen_mode = QComboBox()
        self.pen_mode.addItems(
            [
                "draw: contact holds the left button",
                "move: aim only, click with the keyboard",
                "off: ignore the pen",
            ]
        )
        self.pen_mode.setCurrentIndex(1)
        self.pen_mode.currentIndexChanged.connect(self.on_pen_mode)
        pl.addRow("Action", self.pen_mode)

        self.area = QSlider(Qt.Horizontal)
        self.area.setRange(20, 100)
        self.area.setValue(50)
        self.area.valueChanged.connect(self.on_area_slider)
        self.area_spin = QDoubleSpinBox()
        self.area_spin.setRange(0.20, 1.00)
        self.area_spin.setSingleStep(0.05)
        self.area_spin.setDecimals(2)
        self.area_spin.setValue(0.50)
        self.area_spin.setSuffix(" of the surface")
        self.area_spin.valueChanged.connect(self.on_area_spin)
        arow = QHBoxLayout()
        arow.addWidget(self.area, 1)
        arow.addWidget(self.area_spin)
        pl.addRow("Active area", arow)

        presets = QHBoxLayout()
        for label, value in [("100%", 1.0), ("60%", 0.6), ("50%", 0.5), ("40%", 0.4), ("30%", 0.3)]:
            b = QPushButton(label)
            b.clicked.connect(lambda _=False, v=value: self.set_area(v))
            presets.addWidget(b)
        b = QPushButton("16:9 playfield")
        b.clicked.connect(self.set_playfield)
        presets.addWidget(b)
        pl.addRow("Presets", presets)

        crow = QHBoxLayout()
        self.custom = QLineEdit()
        self.custom.setPlaceholderText("custom x,y,w,h   e.g. 0,0.05,1,0.9")
        apply_custom = QPushButton("Apply")
        apply_custom.clicked.connect(self.apply_custom)
        crow.addWidget(self.custom, 1)
        crow.addWidget(apply_custom)
        pl.addRow("Custom", crow)
        self.absolute_widgets = [self.area, self.area_spin, self.custom]
        self.absolute_widgets += [
            presets.itemAt(i).widget() for i in range(presets.count())
        ] + [apply_custom]
        self.area_status = QLabel("whole surface")
        self.area_status.setStyleSheet("color:#888")
        pl.addRow("", self.area_status)
        outer.addWidget(pen)

        # --- quality -------------------------------------------------------
        q = compact(QGroupBox("Quality"))
        ql = QFormLayout(q)
        self.crf = QSpinBox()
        self.crf.setRange(12, 30)
        self.crf.setValue(16)
        self.crf.setToolTip("Lower is sharper and costs bitrate (16 default here)")
        self.bitrate = QSpinBox()
        self.bitrate.setRange(5, 100)
        self.bitrate.setValue(40)
        self.bitrate.setSuffix(" Mbit/s cap")
        self.preset = QComboBox()
        self.preset.addItems(
            ["ultrafast", "superfast", "veryfast", "faster", "fast", "medium"]
        )
        self.preset.setCurrentText("superfast")
        ql.addRow("CRF", self.crf)
        ql.addRow("Bitrate", self.bitrate)
        ql.addRow("Preset", self.preset)
        outer.addWidget(q)
        outer.addStretch(1)

        # --- run -----------------------------------------------------------
        brow = QHBoxLayout()
        self.start_btn = QPushButton("Start")
        self.start_btn.clicked.connect(self.start)
        self.stop_btn = QPushButton("Stop")
        self.stop_btn.setEnabled(False)
        self.stop_btn.clicked.connect(self.stop)
        self.status = QLabel("idle")
        brow.addWidget(self.start_btn)
        brow.addWidget(self.stop_btn)
        brow.addWidget(self.status, 1)
        outer.addLayout(brow)

        self.log = QPlainTextEdit()
        self.log.setReadOnly(True)
        self.log.setMaximumBlockCount(500)
        self.log.setFixedHeight(170)
        f = QFont("monospace")
        f.setStyleHint(QFont.Monospace)
        self.log.setFont(f)
        outer.addWidget(self.log)

        self.setCentralWidget(root)
        self.resize(760, 820)
        self.load_monitors()
        self.load()
        self.loading = False
        QTimer.singleShot(50, self.refresh_receivers)

    # --- receivers / monitors ---------------------------------------------
    def refresh_receivers(self):
        self.refresh_btn.setEnabled(False)
        self.receiver.clear()
        self.receiver.addItem("searching…")
        t = {0: "auto", 1: "usb", 2: "wifi"}[self.transport.currentIndex()]
        self.worker = ReceiverList(t)
        self.worker.done.connect(self.got_receivers)
        self.worker.start()

    def got_receivers(self, items):
        self.receivers = items
        self.receiver.clear()
        if not items:
            self.receiver.addItem("none found")
        for r in items:
            label = r["name"] + (f"  {r['addr']}" if r["addr"] else "  (USB)")
            self.receiver.addItem(label, r)
        self.refresh_btn.setEnabled(True)

    def load_monitors(self):
        self.monitor.clear()
        try:
            out = subprocess.run(
                ["hyprctl", "monitors", "-j"], capture_output=True, text=True, timeout=2
            )
            for m in json.loads(out.stdout or "[]"):
                if m["name"].startswith("od-"):
                    continue
                self.monitor.addItem(
                    f"{m['name']}  ({m['width']}x{m['height']})", m["name"]
                )
        except Exception:
            pass
        if self.monitor.count() == 0:
            self.monitor.addItem("no monitors found")
        saved = self.settings.value("monitor")
        if saved:
            for i in range(self.monitor.count()):
                if self.monitor.itemData(i) == saved:
                    self.monitor.setCurrentIndex(i)
                    break

    def sync_mode(self):
        tablet = self.mode_tablet.isChecked()
        self.monitor.setEnabled(tablet)

    # --- pen ---------------------------------------------------------------
    def on_pen_mode(self):
        self.save()
        if self.proc and self.proc.state() == QProcess.Running:
            r = Control.send(
                "pencil-mode " + ["draw", "move", "off"][self.pen_mode.currentIndex()]
            )
            if r:
                self.note(f"control: {r}")

    def on_motion(self):
        self.save()
        self.sync_motion()
        if self.proc and self.proc.state() == QProcess.Running:
            r = Control.send(
                "pencil-motion " + ["absolute", "relative"][self.motion.currentIndex()]
            )
            if r:
                self.note(f"control: {r}")

    def on_sensitivity(self):
        self.save()
        if self.proc and self.proc.state() == QProcess.Running:
            Control.send(f"pencil-sensitivity {self.sensitivity.value():.2f}")

    def sync_motion(self):
        relative = self.motion.currentIndex() == 1
        for w in self.absolute_widgets:
            w.setEnabled(not relative)
        self.sensitivity.setEnabled(relative)

    def on_area_slider(self, v):
        self.area_spin.blockSignals(True)
        self.area_spin.setValue(v / 100.0)
        self.area_spin.blockSignals(False)
        self.area_timer.start()

    def on_area_spin(self, v):
        self.area.blockSignals(True)
        self.area.setValue(round(v * 100))
        self.area.blockSignals(False)
        self.area_timer.start()

    def set_area(self, value: float):
        self.area_spin.setValue(value)
        self.area.setValue(round(value * 100))
        self.apply_area()

    def set_playfield(self):
        # 16:9 band centred in the chosen monitor.
        try:
            w, h = self.monitor.currentText().split("(")[1].rstrip(")").split("x")
            w, h = int(w), int(h)
        except Exception:
            return
        ph = min(h, round(w * 9 / 16))
        y0 = (h - ph) / 2 / h
        spec = f"0,{y0:.3f},1,{ph / h:.3f}"
        self.custom.setText(spec)
        self.apply_custom()

    def apply_custom(self):
        spec = self.custom.text().strip()
        if not spec:
            return
        if self.proc and self.proc.state() == QProcess.Running:
            r = Control.send("pencil-area " + spec)
            self.area_status.setText(r or "not applied (sender has no control socket)")
            self.note(f"control: {r}" if r else "control: no socket")
        else:
            self.area_status.setText(f"will use {spec}")
        self.save()

    def apply_area(self):
        value = self.area_spin.value()
        self.area_status.setText(f"{value:.2f} of the surface")
        self.save()
        if self.proc and self.proc.state() == QProcess.Running:
            r = Control.send(f"pencil-area {value:.3f}")
            if r and not r.startswith("ok"):
                self.note(f"control: {r}")

    # --- process -----------------------------------------------------------
    def start(self):
        if self.proc and self.proc.state() == QProcess.Running:
            return
        args = ["run"]
        t = self.transport.currentIndex()
        rec = self.receiver.currentData()
        if t == 1:
            args.append("--usb")
        elif t == 2 and rec and rec.get("addr"):
            args += ["--to", rec["addr"]]
        else:
            args.append("--auto")

        if self.mode_tablet.isChecked():
            mon = self.monitor.currentData()
            args.append("--tablet")
            if mon:
                args.append(mon)
        args += ["--pencil", ["draw", "move", "off"][self.pen_mode.currentIndex()]]
        args += ["--pencil-motion", ["absolute", "relative"][self.motion.currentIndex()]]
        args += ["--pencil-sensitivity", f"{self.sensitivity.value():.2f}"]
        args += ["--pencil-area", f"{self.area_spin.value():.3f}"]
        args += [
            "--crf",
            str(self.crf.value()),
            "--bitrate-mbps",
            str(self.bitrate.value()),
            "--preset",
            self.preset.currentText(),
        ]
        args += ["--control-socket", SOCKET]

        self.note("$ " + " ".join([SENDER, *args]))
        self.proc = QProcess(self)
        self.proc.setProcessChannelMode(QProcess.MergedChannels)
        self.proc.readyReadStandardOutput.connect(self.read_output)
        self.proc.finished.connect(self.finished)
        self.proc.start(SENDER, args)
        self.start_btn.setEnabled(False)
        self.stop_btn.setEnabled(True)
        self.status.setText("starting…")
        self.save()

    def stop(self):
        if self.proc and self.proc.state() != QProcess.NotRunning:
            self.proc.terminate()  # SIGTERM: removes the virtual display
            self.status.setText("stopping…")

    def finished(self, code, _status):
        self.start_btn.setEnabled(True)
        self.stop_btn.setEnabled(False)
        self.status.setText(f"stopped (exit {code})")

    def read_output(self):
        if not self.proc:
            return
        text = bytes(self.proc.readAllStandardOutput()).decode(errors="replace")
        for line in text.splitlines():
            self.note(line)
            if "connected to receiver" in line:
                self.status.setText("connected")
            elif "hello:" in line and "device Some" in line:
                pass
            elif "another od-sender" in line or "ERROR" in line:
                self.status.setText("error")
            elif "dial " in line and "WARN" in line:
                self.status.setText("retrying…")
            elif "PHONE-STATS" in line:
                try:
                    stats = json.loads(line.split("PHONE-STATS ", 1)[1])
                    self.status.setText(
                        f"{stats.get('transport','?')}  {stats.get('fps',0)} fps  "
                        f"{stats.get('mbps',0)} Mbit/s  rtt {stats.get('rtt',0)} ms"
                    )
                except Exception:
                    pass

    def note(self, line: str):
        self.log.appendPlainText(line)

    def closeEvent(self, event):
        self.stop()
        if self.proc:
            self.proc.waitForFinished(3000)
        event.accept()

    # --- persistence --------------------------------------------------------
    def save(self):
        if self.loading:
            return  # widget signals fire while load() is assigning values
        s = self.settings
        s.setValue("transport", self.transport.currentIndex())
        s.setValue("tablet", self.mode_tablet.isChecked())
        if self.monitor.currentData() is not None:
            s.setValue("monitor", self.monitor.currentData())
        s.setValue("pen_mode", self.pen_mode.currentIndex())
        s.setValue("motion", self.motion.currentIndex())
        s.setValue("sensitivity", self.sensitivity.value())
        s.setValue("area", self.area_spin.value())
        s.setValue("crf", self.crf.value())
        s.setValue("bitrate", self.bitrate.value())
        s.setValue("preset", self.preset.currentText())

    def load(self):
        s = self.settings
        self.transport.setCurrentIndex(int(s.value("transport", 0)))
        self.mode_tablet.setChecked(bool(s.value("tablet", False, type=bool)))
        self.mode_extend.setChecked(not self.mode_tablet.isChecked())
        self.pen_mode.setCurrentIndex(int(s.value("pen_mode", 1)))
        self.motion.setCurrentIndex(int(s.value("motion", 0)))
        self.sensitivity.setValue(float(s.value("sensitivity", 1.0)))
        self.area_spin.setValue(float(s.value("area", 0.5)))
        self.area.setValue(round(self.area_spin.value() * 100))
        self.crf.setValue(int(s.value("crf", 16)))
        self.bitrate.setValue(int(s.value("bitrate", 40)))
        self.preset.setCurrentText(str(s.value("preset", "superfast")))
        self.sync_mode()
        self.sync_motion()


def main() -> int:
    app = QApplication(sys.argv)
    app.setApplicationName("OpenDisplay")
    w = Window()
    w.show()
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
