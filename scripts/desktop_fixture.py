#!/usr/bin/env python3
"""Disposable GTK test window. Logs only interaction with this fixture, never other apps.
Requires Python GObject bindings and GTK 3; not needed by lcu itself.
"""
import argparse
import json
from pathlib import Path
import gi

gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, Gdk, GLib

args = argparse.ArgumentParser(description=__doc__)
args.add_argument('--no-focus', action='store_true', help='Map without requesting focus')
args.add_argument('--state-file', type=Path, help='Independent fixture-local JSON state')
args.add_argument('--title', default='Native Computer Use — Disposable Test Window')
args = args.parse_args()
ROOT = Path(__file__).resolve().parents[1]
STATE_PATH = args.state_file or ROOT / '.local' / 'fixture-state.json'
STATE_PATH.parent.mkdir(parents=True, exist_ok=True)
state = {'clicks': 0, 'text': '', 'scrolls': 0, 'drags': 0, 'keys': [], 'held': [],
         'key_events': [], 'shortcuts': [], 'ticks': 0}

def save():
    temporary = STATE_PATH.with_suffix('.tmp')
    temporary.write_text(json.dumps(state, ensure_ascii=False))
    temporary.replace(STATE_PATH)

window = Gtk.Window(title=args.title)
window.set_focus_on_map(not args.no_focus)
window.set_default_size(900, 700)
window.connect('destroy', Gtk.main_quit)
box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
box.set_border_width(36)
window.add(box)
heading = Gtk.Label(label='NATIVE COMPUTER USE TEST')
box.pack_start(heading, False, False, 0)
box.pack_start(Gtk.Label(label='This window is safe to click, type into, scroll, and drag.'), False, False, 0)
entry = Gtk.Entry()
entry.set_placeholder_text('Paste Unicode here')
entry.connect('changed', lambda e: (state.update(text=e.get_text()), save()))
box.pack_start(entry, False, False, 0)
button = Gtk.Button(label='CLICK TARGET — count: 0')
def clicked(button):
    state['clicks'] += 1
    button.set_label('CLICK TARGET — count: ' + str(state['clicks']))
    save()
button.connect('clicked', clicked)
box.pack_start(button, False, False, 0)
area = Gtk.EventBox()
area.set_visible_window(True)
area.set_above_child(True)
area.add(Gtk.Label(label='SCROLL / DRAG TARGET\nDrag anywhere inside this large box.'))
area.set_size_request(700, 250)
area.add_events(Gdk.EventMask.SCROLL_MASK | Gdk.EventMask.SMOOTH_SCROLL_MASK |
                Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.BUTTON_RELEASE_MASK |
                Gdk.EventMask.POINTER_MOTION_MASK)
start = None
moved = False

def press(widget, event):
    global start, moved
    start = (event.x, event.y)
    moved = False
    return True

def motion(widget, event):
    global moved
    if start and (abs(event.x - start[0]) + abs(event.y - start[1])) > 20:
        moved = True
    return True

def release(widget, event):
    global start, moved
    if start and moved:
        state['drags'] += 1
    start = None
    save()
    return True

def scroll(widget, event):
    state['scrolls'] += 1
    save()
    return True

area.connect('button-press-event', press)
area.connect('motion-notify-event', motion)
area.connect('button-release-event', release)
area.connect('scroll-event', scroll)
box.pack_start(area, True, True, 0)
box.pack_start(Gtk.Label(label='Close this window after testing. No changes outside this fixture.'), False, False, 0)

def key(widget, event, pressed):
    name = Gdk.keyval_name(event.keyval) or str(event.keyval)
    state['key_events'] = (state['key_events'] + [{
        'pressed': pressed, 'key': name, 'modifiers': int(event.state),
        'hardware_keycode': int(event.hardware_keycode)}])[-60:]
    if pressed and event.state & Gdk.ModifierType.CONTROL_MASK:
        chord = 'CTRL+' + ('SHIFT+' if event.state & Gdk.ModifierType.SHIFT_MASK else '') + name.upper()
        state['shortcuts'] = (state['shortcuts'] + [chord])[-30:]
    state['keys'] = (state['keys'] + [('down' if pressed else 'up') + ':' + name])[-30:]
    if pressed and name not in state['held']:
        state['held'].append(name)
    elif not pressed and name in state['held']:
        state['held'].remove(name)
    save()
    return False

chooser_button = Gtk.Button(label='OPEN DISPOSABLE DIRECTORY CHOOSER')
def chooser_clicked(button):
    chooser = Gtk.FileChooserDialog(title='LCU Fixture Directory Chooser',
        transient_for=window, action=Gtk.FileChooserAction.SELECT_FOLDER)
    chooser.add_buttons(Gtk.STOCK_CANCEL, Gtk.ResponseType.CANCEL,
                        Gtk.STOCK_OPEN, Gtk.ResponseType.OK)
    chooser.connect('key-press-event', lambda w, e: key(w, e, True))
    chooser.connect('key-release-event', lambda w, e: key(w, e, False))
    chooser.run()
    chooser.destroy()
chooser_button.connect('clicked', chooser_clicked)
box.pack_start(chooser_button, False, False, 0)
indicator = Gtk.Label(label='Held-key visual ticks: 0')
box.pack_start(indicator, False, False, 0)
def tick():
    if state['held']:
        state['ticks'] += 1
        indicator.set_label('Held-key visual ticks: ' + str(state['ticks']))
        save()
    return True
GLib.timeout_add(100, tick)

window.connect('key-press-event', lambda w, e: key(w, e, True))
window.connect('key-release-event', lambda w, e: key(w, e, False))
window.show_all()
if not args.no_focus:
    window.present()
save()
print('FIXTURE_READY', flush=True)
Gtk.main()
