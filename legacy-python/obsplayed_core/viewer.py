"""Write a self-contained HTML page to review a session and jump to highlights."""
from __future__ import annotations

import html
import json
from pathlib import Path
from urllib.parse import quote

TEMPLATE = """<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<title>OBSPlayed - __TITLE__</title>
<style>
  :root { color-scheme: dark; --accent: #0ea5e9; --hot: #f97316; }
  body { margin: 0; font: 14px/1.4 system-ui, sans-serif; background: #0b0f14; color: #e5e7eb; }
  header { padding: 14px 20px; border-bottom: 1px solid #1f2937; display: flex; gap: 12px; align-items: baseline; }
  header h1 { font-size: 18px; margin: 0; } header span { color: #9ca3af; }
  main { display: grid; grid-template-columns: 1fr 320px; gap: 16px; padding: 16px 20px; }
  video { width: 100%; background: #000; border-radius: 8px; }
  #timeline { position: relative; height: 26px; margin-top: 10px; background: #111827; border-radius: 6px; cursor: pointer; }
  #timeline .hl { position: absolute; top: 0; bottom: 0; background: rgba(249,115,22,.35); border-radius: 4px; }
  #timeline .mk { position: absolute; top: 5px; width: 3px; height: 16px; margin-left: -1px; background: var(--accent); border-radius: 2px; }
  #timeline .mk.death { background: #6b7280; } #timeline .mk.big { background: var(--hot); }
  #timeline .ph { position: absolute; top: 0; bottom: 0; width: 2px; background: #fff; pointer-events: none; }
  aside h2 { font-size: 13px; text-transform: uppercase; letter-spacing: .08em; color: #9ca3af; margin: 0 0 8px; }
  ul { list-style: none; margin: 0 0 18px; padding: 0; max-height: 40vh; overflow: auto; }
  li { padding: 8px 10px; border-radius: 6px; cursor: pointer; display: flex; justify-content: space-between; gap: 8px; }
  li:hover { background: #1f2937; } li small { color: #9ca3af; }
  li.h { border-left: 3px solid var(--hot); background: #111827; margin-bottom: 6px; }
  .empty { color: #6b7280; padding: 8px 10px; }
</style></head>
<body>
<header><h1>__TITLE__</h1><span>__SUBTITLE__</span></header>
<main>
  <section>
    <video id="v" controls preload="metadata" src="__VIDEO__"></video>
    <div id="timeline"></div>
  </section>
  <aside>
    <h2>Highlights</h2><ul id="hls"></ul>
    <h2>Events</h2><ul id="mks"></ul>
  </aside>
</main>
<script>
const data = __DATA__;
const v = document.getElementById('v'), tl = document.getElementById('timeline');
const fmt = s => { s = Math.max(0, Math.floor(s)); const h = Math.floor(s/3600), m = Math.floor(s%3600/60), x = String(s%60).padStart(2,'0');
  return h ? `${h}:${String(m).padStart(2,'0')}:${x}` : `${m}:${x}`; };
const seek = t => { v.currentTime = Math.max(0, t); v.play(); };
function list(id, items, cls, render) {
  const ul = document.getElementById(id);
  if (!items.length) { ul.innerHTML = '<div class="empty">Nothing detected</div>'; return; }
  for (const it of items) {
    const li = document.createElement('li'); li.className = cls;
    const [main, side, t] = render(it);
    li.append(Object.assign(document.createElement('span'), {textContent: main}),
              Object.assign(document.createElement('small'), {textContent: side}));
    li.onclick = () => seek(t); ul.append(li);
  }
}
list('hls', data.highlights, 'h', h => [h.title, `${fmt(h.start)} - ${fmt(h.end)}`, h.start]);
list('mks', data.markers, '', m => [m.label + (m.detail ? ` - ${m.detail}` : ''), fmt(m.offset), m.offset - 5]);
function drawTimeline() {
  const d = v.duration || Math.max(1, ...data.markers.map(m => m.offset + 10), ...data.highlights.map(h => h.end));
  tl.innerHTML = '';
  for (const h of data.highlights) {
    const el = document.createElement('div'); el.className = 'hl';
    el.style.left = (h.start / d * 100) + '%'; el.style.width = ((h.end - h.start) / d * 100) + '%'; el.title = h.title; tl.append(el);
  }
  for (const m of data.markers) {
    const el = document.createElement('div');
    el.className = 'mk' + (m.kind === 'death' ? ' death' : m.weight >= 2 ? ' big' : '');
    el.style.left = (m.offset / d * 100) + '%'; el.title = `${fmt(m.offset)} ${m.label}`; tl.append(el);
  }
  const ph = document.createElement('div'); ph.className = 'ph'; ph.id = 'ph'; tl.append(ph);
}
tl.onclick = e => { const r = tl.getBoundingClientRect(); if (v.duration) seek((e.clientX - r.left) / r.width * v.duration); };
v.addEventListener('loadedmetadata', drawTimeline);
v.addEventListener('timeupdate', () => { const ph = document.getElementById('ph'); if (ph && v.duration) ph.style.left = (v.currentTime / v.duration * 100) + '%'; });
drawTimeline();
</script>
</body></html>
"""


def write_viewer(recording: str, data: dict) -> Path:
    rec = Path(recording)
    out = rec.with_name(rec.stem + ".highlights.html")
    title = data.get("game") or "Recording"
    subtitle = f"{data.get('started', '')} · {len(data.get('highlights', []))} highlights · {len(data.get('markers', []))} events"
    payload = json.dumps(data).replace("</", "<\\/")
    page = (
        TEMPLATE.replace("__TITLE__", html.escape(title))
        .replace("__SUBTITLE__", html.escape(subtitle))
        .replace("__VIDEO__", html.escape(quote(rec.name)))
        .replace("__DATA__", payload)
    )
    out.write_text(page, encoding="utf-8")
    return out
