#!/usr/bin/env python3
"""Disposable GTK3/WebKit input and scroll probe; logs only this fixture's events.
Requires Python GI and WebKit2 4.1. Supply graphics overrides at launch, not here.
"""
import argparse
import json
from pathlib import Path
import gi

gi.require_version('Gtk', '3.0')
gi.require_version('WebKit2', '4.1')
from gi.repository import Gtk, Gdk, WebKit2

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--state-file', type=Path, required=True)
args = parser.parse_args()
state = {'native': [], 'dom': [], 'closed': False}

def save():
    temporary = args.state_file.with_suffix('.tmp')
    temporary.write_text(json.dumps(state))
    temporary.replace(args.state_file)

manager = WebKit2.UserContentManager()
manager.register_script_message_handler('probe')
def message(manager, result):
    state['dom'] = (state['dom'] + [json.loads(result.get_js_value().to_string())])[-100:]
    save()
manager.connect('script-message-received::probe', message)
view = WebKit2.WebView.new_with_user_content_manager(manager)
window = Gtk.Window(title='LCU WebKit compatibility fixture')
window.set_default_size(900, 700)
window.add(view)

def key(widget, event):
    state['native'] = (state['native'] + [{'type': 'key', 'key': Gdk.keyval_name(event.keyval),
        'hardware': event.hardware_keycode, 'modifiers': int(event.state)}])[-100:]
    save()
    return False

def scroll(widget, event):
    ok, dx, dy = event.get_scroll_deltas()
    state['native'] = (state['native'] + [{'type': 'scroll', 'smooth': ok, 'dx': dx, 'dy': dy}])[-100:]
    save()
    return False
view.add_events(Gdk.EventMask.SCROLL_MASK | Gdk.EventMask.SMOOTH_SCROLL_MASK)
view.connect('key-press-event', key)
view.connect('scroll-event', scroll)
def closed(widget):
    state['closed'] = True
    save()
    Gtk.main_quit()
window.connect('destroy', closed)
view.load_html('''<!doctype html><meta charset="utf-8"><style>
body{font:20px sans-serif;background:#eef4f7;color:#142e3e;padding:24px}
button,input{font:inherit;margin:10px;padding:10px}
#scroll{height:360px;overflow:auto;border:3px solid #327582}
#inside{height:1800px;background:repeating-linear-gradient(#dae8ef 0 99px,#639caf 100px 101px);padding:20px}
</style><h1>LCU WebKit compatibility fixture</h1>
<button id="first">First focus target</button><button id="second">Second focus target</button>
<p id="position">Waiting</p><div id="scroll"><div id="inside">Scrollable target — starts in the middle</div></div>
<script>
const box=document.querySelector('#scroll');
function report(type,extra={}){window.webkit.messageHandlers.probe.postMessage(JSON.stringify({type,
 scrollTop:box.scrollTop,focus:document.activeElement.id,...extra}));}
document.addEventListener('keydown',e=>report('key',{key:e.key,code:e.code,shift:e.shiftKey}));
box.addEventListener('wheel',e=>report('wheel',{dx:e.deltaX,dy:e.deltaY}),{passive:true});
box.addEventListener('scroll',()=>{document.querySelector('#position').textContent='Scroll offset: '+box.scrollTop;report('scroll');});
document.addEventListener('focusin',()=>report('focus'));
box.scrollTop=650;document.querySelector('#second').focus();report('ready');
</script>''', 'about:blank')
window.show_all()
window.present()
save()
Gtk.main()
