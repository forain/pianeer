pub(super) const HTML_TEMPLATE: &str = r#"
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1,maximum-scale=1,user-scalable=no">
<title>Pianeer</title>
<style>
/* ── Reset & base ─────────────────────────────────────────────────────── */
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
html, body { height: 100%; }
body {
  background: #1a1a1a; color: #e0e0e0;
  font-family: 'Courier New', monospace; font-size: 14px;
  -webkit-tap-highlight-color: transparent;
  touch-action: manipulation;
  overscroll-behavior: none;
  display: flex; flex-direction: column;
}
button { font-family: inherit; }

/* ── Header ───────────────────────────────────────────────────────────── */
#hdr {
  display: flex; align-items: center; gap: 10px;
  padding: 8px 14px; border-bottom: 1px solid #262626;
  flex-shrink: 0; background: #1f1f1f;
}
#hdr h1 { font-size: 18px; color: #80d0ff; }
#status  { font-size: 11px; color: #666; margin-left: auto; }

/* ── App: column on mobile, row on desktop ────────────────────────────── */
#app { flex: 1; display: flex; flex-direction: column; overflow: hidden; }
@media (min-width: 700px) { #app { flex-direction: row; } }

/* ── Main column ──────────────────────────────────────────────────────── */
#main { display: flex; flex-direction: column; flex: 1; overflow: hidden; }
@media (min-width: 700px) { #main { border-right: 1px solid #262626; } }

/* ── VU meters ────────────────────────────────────────────────────────── */
#vu {
  background: #252525; padding: 7px 14px 5px;
  border-bottom: 1px solid #262626; flex-shrink: 0; position: relative;
}
.vrow  { display: flex; align-items: center; gap: 8px; margin: 2px 0; }
.vlbl  { width: 12px; color: #777; font-size: 11px; }
.vtrk  { flex: 1; height: 8px; background: #333; border-radius: 2px; overflow: hidden; }
.vfill { height: 100%; width: 0%; transition: width 60ms linear; }
.vdb   { width: 58px; text-align: right; font-size: 11px; color: #aaa; }
#clip  { position: absolute; top: 9px; right: 14px; color: #e05050; font-weight: bold; font-size: 11px; }
#stats { display: flex; flex-wrap: wrap; gap: 8px; padding: 3px 0 1px; font-size: 11px; color: #666; }

/* ── Settings chips (mobile only, hidden on desktop) ──────────────────── */
#chips {
  display: flex; flex-wrap: wrap; gap: 5px;
  padding: 7px 14px; border-bottom: 1px solid #262626; flex-shrink: 0;
}
@media (min-width: 700px) { #chips { display: none; } }

.chip {
  display: inline-flex; align-items: stretch;
  height: 40px; background: #252525; border: 1px solid #383838;
  border-radius: 6px; overflow: hidden; font-size: 12px;
}
.clbl {
  padding: 0 7px; color: #666; font-size: 11px;
  border-right: 1px solid #383838;
  display: flex; align-items: center; flex-shrink: 0;
}
.ctap {
  padding: 0 10px; min-width: 46px;
  display: flex; align-items: center; justify-content: center;
  cursor: pointer; user-select: none;
}
.ctap:active { background: #2a3a5a; }
.chip.on  .ctap { color: #70ffaa; }
.chip.off .ctap { color: #555; }
.chip.sus .ctap { color: #80d0ff; font-weight: bold; }
/* stepper chip (Vol, Trn) */
.cstep {
  width: 36px; display: flex; align-items: center; justify-content: center;
  cursor: pointer; color: #666; font-size: 17px; user-select: none;
  border-left: 1px solid #383838;
}
.cstep:active { background: #2a3a5a; color: #e0e0e0; }
.cval {
  padding: 0 8px; color: #e0e0e0; font-size: 12px; min-width: 50px;
  border-left: 1px solid #383838; border-right: 1px solid #383838;
  display: flex; align-items: center; justify-content: center;
}

/* ── Playback / recording bar ─────────────────────────────────────────── */
#pb {
  background: #1e2a1e; border-bottom: 1px solid #2a402a;
  padding: 8px 14px; flex-shrink: 0;
}
#pb.hidden { display: none; }
#pb-row1 { display: flex; align-items: center; gap: 10px; margin-bottom: 6px; }
#pb-icon { font-size: 18px; width: 22px; text-align: center; }
#pb-time { font-size: 12px; color: #aaa; white-space: nowrap; }
#pb-controls { display: flex; gap: 5px; margin-left: auto; }
.pb-btn {
  background: #2a3a2a; border: 1px solid #3a5a3a; color: #90e090;
  border-radius: 4px; padding: 5px 10px; cursor: pointer;
  font-size: 12px; font-family: inherit;
}
.pb-btn:active { background: #3a5a3a; }
#pb-bar-wrap {
  height: 6px; background: #2a2a2a; border-radius: 3px;
  cursor: pointer; position: relative;
}
#pb-fill {
  height: 100%; background: #50c050; border-radius: 3px;
  width: 0%; pointer-events: none; transition: width 100ms linear;
}
#rec-bar {
  background: #2a1e1e; border-bottom: 1px solid #5a2a2a;
  padding: 6px 14px; flex-shrink: 0; font-size: 12px;
  color: #e05050; display: none; align-items: center; gap: 8px;
}
#rec-bar.visible { display: flex; }

/* ── Instrument list ──────────────────────────────────────────────────── */
#mscroll { flex: 1; overflow-y: auto; -webkit-overflow-scrolling: touch; }
.slbl {
  padding: 8px 14px 3px; font-size: 11px; color: #555;
  letter-spacing: 1.5px; user-select: none;
}
.item {
  display: flex; align-items: center; gap: 8px;
  padding: 11px 14px; cursor: pointer; user-select: none;
  min-height: 48px; border-bottom: 1px solid #212121;
}
.item:hover  { background: #212121; }
.item:active { background: #1b2b40; }
.item.cursor { background: #1b2b40; box-shadow: inset 3px 0 0 #3a5599; }
.item.loaded  .iname { color: #70c8ff; }
.item.playing .iname { color: #70ffaa; }
.itype { width: 28px; font-size: 11px; color: #555; flex-shrink: 0; margin-right: 4px; }
.iname { flex: 1; font-size: 13px; }
.vara  { display: flex; align-items: center; gap: 3px; flex-shrink: 0; }
.vname { font-size: 11px; color: #777; min-width: 38px; text-align: center; }
.vbtn  {
  background: transparent; border: 1px solid #3a3a3a; color: #666;
  cursor: pointer; font-size: 14px; width: 36px; height: 36px;
  border-radius: 4px; display: flex; align-items: center; justify-content: center;
}
.vbtn:hover  { color: #ccc; border-color: #666; }
.vbtn:active { background: #2a3a5a; }

/* ── Bottom nav bar ───────────────────────────────────────────────────── */
#nav {
  display: flex; gap: 6px; padding: 8px 14px;
  border-top: 1px solid #262626; flex-shrink: 0; background: #252525;
}
.nbtn {
  background: #2e2e2e; border: 1px solid #383838; color: #e0e0e0;
  cursor: pointer; border-radius: 5px; height: 46px;
  display: flex; align-items: center; justify-content: center; user-select: none;
}
.nbtn:active { background: #2a3a5a; }
.nbtn-ico { width: 46px; font-size: 20px; }
#load-btn {
  flex: 1; background: #1b2b40; border-color: #3a5599;
  color: #80d0ff; font-size: 14px; font-weight: bold; letter-spacing: 0.5px;
}
#load-btn:active { background: #243558; }
#rec-btn.recording { background: #3a1a1a; border-color: #cc3333; color: #ff6060; }
.nbtn-txt { padding: 0 14px; font-size: 12px; color: #666; }

/* ── Settings side panel (desktop only) ───────────────────────────────── */
#side {
  width: 240px; flex-shrink: 0; padding: 12px 14px;
  display: flex; flex-direction: column; gap: 6px;
  overflow-y: auto; background: #1c1c1c;
}
@media (max-width: 699px) { #side { display: none; } }

.shdr { font-size: 11px; color: #555; letter-spacing: 1.5px; margin: 4px 0 2px; }
.srow { display: flex; align-items: center; gap: 6px; }
.slb2 { width: 72px; font-size: 12px; color: #777; flex-shrink: 0; }
.sval {
  flex: 1; background: #252525; border: 1px solid #383838;
  border-radius: 4px; padding: 9px 6px; font-size: 12px;
  cursor: pointer; text-align: center; color: #e0e0e0; user-select: none;
}
.sval:hover  { background: #2e2e2e; }
.sval:active { background: #2a3a5a; }
.sval.on  { color: #70ffaa; }
.sval.off { color: #555; }
.sval.sus { color: #80d0ff; font-weight: bold; }
.sval.ro  { cursor: default; pointer-events: none; }
.sbtn {
  background: #252525; border: 1px solid #383838; color: #777;
  border-radius: 4px; width: 38px; height: 38px; cursor: pointer;
  font-size: 18px; display: flex; align-items: center; justify-content: center;
  flex-shrink: 0; user-select: none;
}
.sbtn:hover  { background: #2e2e2e; color: #e0e0e0; }
.sbtn:active { background: #2a3a5a; }
.snum { flex: 1; text-align: center; font-size: 13px; padding: 4px; color: #e0e0e0; }
#sd-rescan {
  margin-top: 4px; padding: 10px; background: #252525;
  border: 1px solid #383838; border-radius: 4px;
  cursor: pointer; text-align: center; font-size: 12px; color: #666; user-select: none;
}
#sd-rescan:hover  { background: #2e2e2e; }
#sd-rescan:active { background: #2a3a5a; }
</style>
</head>
<body>

<div id="hdr">
  <h1>Pianeer</h1>
  <span id="status">Connecting&hellip;</span>
  <button onclick="document.getElementById('qr-modal').style.display='flex'" style="background:none;border:1px solid #383838;border-radius:4px;color:#777;cursor:pointer;padding:3px 8px;font-size:11px;font-family:inherit;flex-shrink:0;">QR</button>
</div>

<div id="app">

  <!-- Main column: VU + chips + list + nav -->
  <div id="main">

    <div id="vu">
      <div class="vrow">
        <span class="vlbl">L</span>
        <div class="vtrk"><div class="vfill" id="vu-l"></div></div>
        <span class="vdb" id="vu-l-db">-&infin;&nbsp;dB</span>
        <span id="clip"></span>
      </div>
      <div class="vrow">
        <span class="vlbl">R</span>
        <div class="vtrk"><div class="vfill" id="vu-r"></div></div>
        <span class="vdb" id="vu-r-db">-&infin;&nbsp;dB</span>
      </div>
      <div id="stats"></div>
    </div>

    <!-- Settings chips (mobile only) — rebuilt by JS -->
    <div id="chips"></div>

    <!-- Recording bar (only visible while recording) -->
    <div id="rec-bar">
      <span>&#9679; REC</span>
      <span id="rec-time">00:00</span>
      <button class="pb-btn" data-cmd="ToggleRecord" style="margin-left:auto">&#9646;&#9646; Stop</button>
    </div>

    <!-- Playback bar -->
    <div id="pb" class="hidden">
      <div id="pb-row1">
        <span id="pb-icon">&#9654;</span>
        <span id="pb-time">00:00 / 00:00</span>
        <div id="pb-controls">
          <button class="pb-btn" data-json='{"cmd":"SeekRelative","secs":-10}'>&#8722;10s</button>
          <button class="pb-btn" id="pb-pause" data-cmd="PauseResume">&#9646;&#9646;</button>
          <button class="pb-btn" data-json='{"cmd":"SeekRelative","secs":10}'>+10s</button>
        </div>
      </div>
      <div id="pb-bar-wrap">
        <div id="pb-fill"></div>
      </div>
    </div>

    <div id="mscroll"><div id="menu"></div></div>

    <div id="nav">
      <button class="nbtn nbtn-ico" data-cmd="CursorUp">&#8593;</button>
      <button class="nbtn nbtn-ico" data-cmd="CursorDown">&#8595;</button>
      <button class="nbtn" id="load-btn" data-cmd="Select">Load</button>
      <button class="nbtn nbtn-txt" id="rec-btn" data-cmd="ToggleRecord">&#9679; Rec</button>
      <button class="nbtn nbtn-txt" data-cmd="Rescan">Rescan</button>
      <button class="nbtn nbtn-txt" id="audio-btn" onclick="toggleAudio()">&#128266;</button>
    </div>

  </div><!-- #main -->

  <!-- Settings side panel (desktop only) -->
  <div id="side">
    <div class="shdr">SETTINGS</div>
    <div class="srow">
      <span class="slb2">Velocity</span>
      <div class="sval" id="sd-vel" data-cmd="CycleVeltrack"></div>
    </div>
    <div class="srow">
      <span class="slb2">Tune</span>
      <div class="sval" id="sd-tune" data-cmd="CycleTune"></div>
    </div>
    <div class="srow">
      <span class="slb2">Release</span>
      <div class="sval" id="sd-rel" data-cmd="ToggleRelease"></div>
    </div>
    <div class="srow">
      <span class="slb2">Resonance</span>
      <div class="sval" id="sd-res" data-cmd="ToggleResonance"></div>
    </div>
    <div class="srow">
      <span class="slb2">Sustain</span>
      <div class="sval ro" id="sd-sus"></div>
    </div>
    <div class="shdr" style="margin-top:8px">VOLUME</div>
    <div class="srow">
      <button class="sbtn" data-json='{"cmd":"VolumeChange","delta":-1}'>&#8722;</button>
      <div class="snum" id="sd-vol">0&nbsp;dB</div>
      <button class="sbtn" data-json='{"cmd":"VolumeChange","delta":1}'>+</button>
    </div>
    <div class="shdr" style="margin-top:6px">TRANSPOSE</div>
    <div class="srow">
      <button class="sbtn" data-json='{"cmd":"TransposeChange","delta":-1}'>&#8722;</button>
      <div class="snum" id="sd-trn">0</div>
      <button class="sbtn" data-json='{"cmd":"TransposeChange","delta":1}'>+</button>
    </div>
    <button id="sd-rescan" data-cmd="Rescan">Rescan instruments</button>
    <button id="sd-rec" data-cmd="ToggleRecord" style="margin-top:4px;padding:10px;background:#252525;border:1px solid #383838;border-radius:4px;cursor:pointer;text-align:center;font-size:12px;color:#e05050;font-family:inherit;">&#9679;&nbsp;Record</button>
    <button id="sd-audio" onclick="toggleAudio()" style="margin-top:4px;padding:10px;background:#252525;border:1px solid #383838;border-radius:4px;cursor:pointer;text-align:center;font-size:12px;color:#777;font-family:inherit;">&#128266;&nbsp;Monitor</button>
  </div><!-- #side -->

</div><!-- #app -->

<script>
'use strict';
const CERT_HASH = new Uint8Array(__CERT_HASH__);
const enc = new TextEncoder(), dec = new TextDecoder();
let doSend = null;

function sendRaw(obj) { if (doSend) doSend(obj); }
function sendCmd(c)   { sendRaw({ cmd: c }); }

// ── Event delegation (works across innerHTML rebuilds) ─────────────────────
document.addEventListener('click', e => {
  const el = e.target.closest('[data-cmd],[data-json]');
  if (!el) return;
  const c = el.dataset.cmd, j = el.dataset.json;
  if (c) sendCmd(c); else if (j) sendRaw(JSON.parse(j));
});

// Menu: pointerdown for variant arrows (beats the 100ms re-render),
//       click for row selection.
const menuEl = document.getElementById('menu');
menuEl.addEventListener('pointerdown', e => {
  const btn = e.target.closest('[data-varidx]');
  if (!btn) return;
  e.preventDefault();
  sendRaw({ cmd: 'CycleVariantAt', idx: +btn.dataset.varidx, dir: +btn.dataset.vardir });
});
menuEl.addEventListener('click', e => {
  if (e.target.closest('[data-varidx]')) return;
  const row = e.target.closest('[data-selidx]');
  if (row) sendRaw({ cmd: 'SelectAt', idx: +row.dataset.selidx });
});

// ── Keyboard shortcuts ─────────────────────────────────────────────────────
document.addEventListener('keydown', e => {
  if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
  const k = e.key;
  if      (k === 'ArrowUp')        { e.preventDefault(); sendCmd('CursorUp'); }
  else if (k === 'ArrowDown')      { e.preventDefault(); sendCmd('CursorDown'); }
  else if (k === 'ArrowLeft')      sendRaw({ cmd: 'CycleVariant', dir: -1 });
  else if (k === 'ArrowRight')     sendRaw({ cmd: 'CycleVariant', dir: 1 });
  else if (k === 'Enter')          sendCmd('Select');
  else if (k === 'v' || k === 'V') sendCmd('CycleVeltrack');
  else if (k === 't' || k === 'T') sendCmd('CycleTune');
  else if (k === 'e' || k === 'E') sendCmd('ToggleRelease');
  else if (k === '+' || k === '=') sendRaw({ cmd: 'VolumeChange',    delta: 1 });
  else if (k === '-')              sendRaw({ cmd: 'VolumeChange',    delta: -1 });
  else if (k === '[')              sendRaw({ cmd: 'TransposeChange', delta: -1 });
  else if (k === ']')              sendRaw({ cmd: 'TransposeChange', delta: 1 });
  else if (k === 'h' || k === 'H') sendCmd('ToggleResonance');
  else if (k === 'w' || k === 'W') sendCmd('ToggleRecord');
  else if (k === ' ')              { e.preventDefault(); sendCmd('PauseResume'); }
  else if (k === ',')              sendRaw({ cmd: 'SeekRelative', secs: -10 });
  else if (k === '.')              sendRaw({ cmd: 'SeekRelative', secs:  10 });
  else if (k === 'r' || k === 'R') sendCmd('Rescan');
});

// ── Seekbar click → seek ───────────────────────────────────────────────────
document.getElementById('pb-bar-wrap').addEventListener('click', e => {
  const rect = e.currentTarget.getBoundingClientRect();
  const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
  const totalUs = renderState._totalUs || 0;
  const secs = Math.round((frac * totalUs / 1_000_000) - (totalUs / 2_000_000));
  // Compute absolute position and send as SeekRelative from current
  const curUs = renderState._curUs || 0;
  const targetUs = frac * totalUs;
  const deltaSecs = Math.round((targetUs - curUs) / 1_000_000);
  sendRaw({ cmd: 'SeekRelative', secs: deltaSecs });
});

// ── VU helpers ─────────────────────────────────────────────────────────────
function vuPct(peak) {
  if (peak < 1e-10) return 0;
  return Math.max(0, Math.min(100, ((20 * Math.log10(peak) + 48) / 48) * 100));
}
function vuColor(pct) {
  return pct > 95 ? '#cc3333' : pct > 75 ? '#ccaa22' : '#2a8040';
}
function vuDb(peak) {
  return peak < 1e-10 ? '-\u221e\u00a0dB' : (20 * Math.log10(peak)).toFixed(1) + '\u00a0dB';
}
function esc(s) {
  return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
}

// ── Chip builders ──────────────────────────────────────────────────────────
function mkChip(lbl, val, cmd, cls) {
  return `<span class="chip ${cls}">` +
    `<span class="clbl">${lbl}</span>` +
    `<span class="ctap" data-cmd="${cmd}">${esc(String(val))}</span>` +
    `</span>`;
}
function mkChipRo(lbl, val, cls) {
  return `<span class="chip ${cls}">` +
    `<span class="clbl">${lbl}</span>` +
    `<span class="ctap" style="cursor:default">${esc(String(val))}</span>` +
    `</span>`;
}
function mkStepper(lbl, val, jMinus, jPlus) {
  return `<span class="chip">` +
    `<span class="clbl">${lbl}</span>` +
    `<span class="cstep" data-json='${jMinus}'>&#8722;</span>` +
    `<span class="cval">${esc(String(val))}</span>` +
    `<span class="cstep" data-json='${jPlus}'>+</span>` +
    `</span>`;
}

function fmtTime(us) {
  const s = Math.floor(us / 1_000_000);
  return String(Math.floor(s/60)).padStart(2,'0') + ':' + String(s%60).padStart(2,'0');
}

// ── Main render ────────────────────────────────────────────────────────────
function renderState(st) {
  const s = st.settings, stats = st.stats;
  const vol = s.volume_db === 0 ? '0\u00a0dB'
    : (s.volume_db > 0 ? '+' : '') + s.volume_db.toFixed(0) + '\u00a0dB';
  const trn = s.transpose === 0 ? '0'
    : (s.transpose > 0 ? '+' : '') + s.transpose;

  // VU
  function setVU(fId, dId, peak) {
    const f = document.getElementById(fId), pct = vuPct(peak);
    if (f) { f.style.width = pct + '%'; f.style.background = vuColor(pct); }
    const d = document.getElementById(dId);
    if (d) d.textContent = vuDb(peak);
  }
  setVU('vu-l', 'vu-l-db', stats.peak_l);
  setVU('vu-r', 'vu-r-db', stats.peak_r);
  document.getElementById('clip').textContent = stats.clip ? '[CLIP]' : '';
  document.getElementById('stats').innerHTML =
    `<span>CPU\u00a0${stats.cpu_pct}%</span>` +
    `<span>Mem\u00a0${stats.mem_mb}\u00a0MB</span>` +
    `<span>Voices\u00a0${stats.voices}</span>`;

  // Recording bar + nav/side buttons
  const recBar = document.getElementById('rec-bar');
  if (recBar) recBar.classList.toggle('visible', !!st.recording);
  const recTimeEl = document.getElementById('rec-time');
  if (recTimeEl && st.recording) {
    recTimeEl.textContent = st.rec_elapsed_us != null ? fmtTime(st.rec_elapsed_us) : '00:00';
  }
  const recBtn = document.getElementById('rec-btn');
  if (recBtn) {
    recBtn.classList.toggle('recording', !!st.recording);
    recBtn.innerHTML = st.recording ? '&#9646;&#9646;&nbsp;Stop' : '&#9679;&nbsp;Rec';
  }
  const sdRec = document.getElementById('sd-rec');
  if (sdRec) {
    sdRec.textContent = st.recording ? '\u23F8 Stop Rec' : '\u25CF Record';
    sdRec.style.color  = st.recording ? '#ff6060' : '#e05050';
    sdRec.style.background = st.recording ? '#3a1a1a' : '#252525';
  }

  // Playback bar
  const pbEl = document.getElementById('pb');
  if (pbEl) {
    const pb = st.playback;
    if (pb && pb.total_us > 0) {
      pbEl.classList.remove('hidden');
      renderState._curUs   = pb.current_us;
      renderState._totalUs = pb.total_us;
      const pct = Math.min(100, pb.current_us / pb.total_us * 100);
      const fill = document.getElementById('pb-fill');
      if (fill) fill.style.width = pct + '%';
      const timeEl = document.getElementById('pb-time');
      if (timeEl) timeEl.textContent = fmtTime(pb.current_us) + ' / ' + fmtTime(pb.total_us);
      const icon = document.getElementById('pb-icon');
      if (icon) icon.textContent = pb.paused ? '\u23F8' : '\u25B6';
      const pauseBtn = document.getElementById('pb-pause');
      if (pauseBtn) pauseBtn.innerHTML = pb.paused ? '&#9654;' : '&#9646;&#9646;';
    } else {
      pbEl.classList.add('hidden');
      renderState._curUs   = 0;
      renderState._totalUs = 0;
    }
  }

  // Desktop side panel
  function sd(id, text, cls) {
    const el = document.getElementById(id);
    if (!el) return;
    el.textContent = text;
    el.className = 'sval ' + cls + (el.classList.contains('ro') ? ' ro' : '');
  }
  sd('sd-vel',  s.veltrack,                        '');
  sd('sd-tune', s.tune,                            '');
  sd('sd-rel',  s.release_enabled   ? 'on':'off',  s.release_enabled   ? 'on':'off');
  sd('sd-res',  s.resonance_enabled ? 'on':'off',  s.resonance_enabled ? 'on':'off');
  sd('sd-sus',  st.sustain          ? 'ON':'off',  st.sustain          ? 'sus':'off');
  const sdVol = document.getElementById('sd-vol');
  if (sdVol) sdVol.textContent = vol;
  const sdTrn = document.getElementById('sd-trn');
  if (sdTrn) sdTrn.textContent = trn;

  // Mobile chips — only rebuild when settings/sustain change
  const ck = s.veltrack + '|' + s.tune + '|' + s.release_enabled + '|' +
             s.resonance_enabled + '|' + s.volume_db + '|' + s.transpose + '|' + st.sustain;
  if (ck !== renderState._ck) {
    renderState._ck = ck;
    const relC = s.release_enabled   ? 'on' : 'off';
    const resC = s.resonance_enabled ? 'on' : 'off';
    const susC = st.sustain          ? 'sus': 'off';
    document.getElementById('chips').innerHTML =
      mkChip('Vel',  s.veltrack,                        'CycleVeltrack',   '') +
      mkChip('Tune', s.tune,                            'CycleTune',       '') +
      mkChip('Rel',  s.release_enabled   ? 'on':'off',  'ToggleRelease',   relC) +
      mkChip('Res',  s.resonance_enabled ? 'on':'off',  'ToggleResonance', resC) +
      mkStepper('Vol', vol,
        '{"cmd":"VolumeChange","delta":-1}',
        '{"cmd":"VolumeChange","delta":1}') +
      mkStepper('Trn', trn,
        '{"cmd":"TransposeChange","delta":-1}',
        '{"cmd":"TransposeChange","delta":1}') +
      mkChipRo('Sus', st.sustain ? 'ON' : 'off', susC);
  }

  // Menu — only rebuild when list content changes
  const mk = JSON.stringify(st.menu);
  if (mk !== renderState._mk) {
    renderState._mk = mk;
    let html = '', inMidi = false, shownInst = false;
    st.menu.forEach((item, idx) => {
      if (item.type_label !== 'MID' && !shownInst) {
        html += '<div class="slbl">INSTRUMENTS</div>';
        shownInst = true;
      }
      if (item.type_label === 'MID' && !inMidi) {
        html += '<div class="slbl">MIDI FILES</div>';
        inMidi = true;
      }
      const cls = ['item',
        item.cursor  ? 'cursor'  : '',
        item.loaded  ? 'loaded'  : '',
        item.playing ? 'playing' : '',
      ].filter(Boolean).join(' ');
      const varHtml = item.variant
        ? `<span class="vara">` +
          `<button class="vbtn" data-varidx="${idx}" data-vardir="-1">\u25c4</button>` +
          `<span class="vname">${esc(item.variant)}</span>` +
          `<button class="vbtn" data-varidx="${idx}" data-vardir="1">\u25ba</button>` +
          `</span>`
        : '';
      html +=
        `<div class="${cls}" data-selidx="${idx}">` +
        `<span class="itype">${esc(item.type_label)}</span>` +
        `<span class="iname">${esc(item.name)}</span>` +
        varHtml + `</div>`;
    });
    menuEl.innerHTML = html;
  }
}

// ── Audio monitoring ───────────────────────────────────────────────────────
// Primary path:  WebTransport datagrams  (low latency, unreliable)
// Fallback path: /audio-ws WebSocket     (reliable, slightly higher latency)
// Both carry FLAC-encoded stereo 16-bit 48 kHz in 256-sample blocks.

const AUDIO_BLOCK = 256, AUDIO_SR = 48000;

const WORKLET_SRC = `
class FlacRingProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    const C = 1 << 16; // 65536 samples ≈ 1.4 s at 48 kHz
    this._L = new Float32Array(C); this._R = new Float32Array(C);
    this._mask = C - 1; this._w = 0; this._r = 0; this._started = false;
    this._TARGET = 5 * 256; // buffer 5 blocks before starting output (~26 ms)
    this.port.onmessage = ({data: {L, R}}) => {
      for (let i = 0; i < L.length; i++) {
        this._L[this._w & this._mask] = L[i];
        this._R[this._w & this._mask] = R[i];
        this._w++;
      }
    };
  }
  process(_, outputs) {
    const out = outputs[0], avail = this._w - this._r;
    if (!this._started) {
      if (avail >= this._TARGET) this._started = true;
      else { out[0].fill(0); out[1].fill(0); return true; }
    }
    if (avail < out[0].length) {
      this._started = false; out[0].fill(0); out[1].fill(0); return true; // underrun → rebuffer
    }
    for (let i = 0; i < out[0].length; i++) {
      out[0][i] = this._L[this._r & this._mask];
      out[1][i] = this._R[this._r & this._mask];
      this._r++;
    }
    return true;
  }
}
registerProcessor('flac-ring', FlacRingProcessor);
`;

let _audioCtx = null, _workletNode = null, _decoder = null;
let _audioActive = false, _audioWs = null, _audioFrameTs = 0;
let _wtObj = null; // set to the live WebTransport object by connect()

async function toggleAudio() {
  if (_audioActive) { stopAudio(); return; }
  _audioActive = true;
  updateAudioBtns();
  try { await startAudio(); }
  catch(e) {
    console.error('Audio start failed:', e);
    _audioActive = false;
    updateAudioBtns();
    alert('Audio monitoring requires Chrome/Edge 94+ with WebCodecs support.');
  }
}

async function startAudio() {
  if (!window.isSecureContext || typeof AudioDecoder === 'undefined' || typeof AudioWorklet === 'undefined') {
    // Non-secure context (HTTP on LAN, Android, etc.): use raw PCM + ScriptProcessorNode.
    // No WebCodecs or AudioWorklet needed.
    await startAudioPcm();
    return;
  }
  // Secure context: full FLAC + AudioWorklet path.
  _audioCtx = new AudioContext({sampleRate: AUDIO_SR, latencyHint: 'interactive'});
  const blob = new Blob([WORKLET_SRC], {type: 'application/javascript'});
  await _audioCtx.audioWorklet.addModule(URL.createObjectURL(blob));
  _workletNode = new AudioWorkletNode(_audioCtx, 'flac-ring',
    {numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2]});
  _workletNode.connect(_audioCtx.destination);
  if (_wtObj) { sendCmd('RequestAudioInit'); } else { startAudioWs(); }
}

async function startAudioPcm() {
  _audioCtx = new AudioContext({sampleRate: AUDIO_SR, latencyHint: 'interactive'});
  // Ring buffers for L and R channels
  const RBCAP = 1 << 16; // 65536 samples ≈ 1.4 s
  const rbL = new Float32Array(RBCAP), rbR = new Float32Array(RBCAP);
  const rbMask = RBCAP - 1;
  let rbW = 0, rbRd = 0, rbStarted = false;
  const TARGET = 5 * AUDIO_BLOCK;

  // ScriptProcessorNode works in non-secure contexts (deprecated but universally supported)
  const bufSize = 2048;
  const sp = _audioCtx.createScriptProcessor(bufSize, 0, 2);
  sp.onaudioprocess = e => {
    const outL = e.outputBuffer.getChannelData(0);
    const outR = e.outputBuffer.getChannelData(1);
    const avail = rbW - rbRd;
    if (!rbStarted) {
      if (avail >= TARGET) rbStarted = true;
      else { outL.fill(0); outR.fill(0); return; }
    }
    if (avail < outL.length) { rbStarted = false; outL.fill(0); outR.fill(0); return; }
    for (let i = 0; i < outL.length; i++) {
      outL[i] = rbL[rbRd & rbMask];
      outR[i] = rbR[rbRd & rbMask];
      rbRd++;
    }
  };
  sp.connect(_audioCtx.destination);
  _workletNode = sp; // reuse slot so stopAudio() disconnects it

  const ws = new WebSocket(`ws://${location.hostname}:4000/audio-pcm-ws`);
  ws.binaryType = 'arraybuffer';
  _audioWs = ws;
  ws.onmessage = e => {
    // i16 stereo little-endian: [L0_lo, L0_hi, R0_lo, R0_hi, L1_lo, L1_hi, …]
    const view = new DataView(e.data);
    const frames = e.data.byteLength >> 2; // 4 bytes per stereo frame
    for (let i = 0; i < frames; i++) {
      rbL[rbW & rbMask] = view.getInt16(i * 4,     true) / 32767;
      rbR[rbW & rbMask] = view.getInt16(i * 4 + 2, true) / 32767;
      rbW++;
    }
  };
  ws.onerror = ws.onclose = () => {
    if (_audioActive) setTimeout(startAudioPcm, 3000);
  };
}

function stopAudio() {
  _audioActive = false;
  try { _decoder?.close(); } catch(_) {}
  _decoder = null;
  _audioWs?.close(); _audioWs = null;
  try { _workletNode?.disconnect(); } catch(_) {}
  _workletNode = null;
  _audioCtx?.close(); _audioCtx = null;
  updateAudioBtns();
}

function setupDecoder(streaminfo) { // Uint8Array of 34 bytes (raw STREAMINFO payload)
  try { _decoder?.close(); } catch(_) {}
  _decoder = new AudioDecoder({
    output: audioData => {
      if (!_workletNode) { audioData.close(); return; }
      const n = audioData.numberOfFrames;
      const L = new Float32Array(n), R = new Float32Array(n);
      try {
        audioData.copyTo(L, {planeIndex: 0, format: 'f32-planar'});
        audioData.copyTo(R, {planeIndex: 1, format: 'f32-planar'});
      } catch(_) {
        // interleaved fallback
        const tmp = new Float32Array(n * 2);
        audioData.copyTo(tmp, {planeIndex: 0});
        for (let i = 0; i < n; i++) { L[i] = tmp[i*2]; R[i] = tmp[i*2+1]; }
      }
      _workletNode.port.postMessage({L, R}, [L.buffer, R.buffer]);
      audioData.close();
    },
    error: e => console.error('FLAC decoder:', e),
  });
  _decoder.configure({codec: 'flac', sampleRate: AUDIO_SR, numberOfChannels: 2,
    description: streaminfo});
  _audioFrameTs = 0;
}

function feedAudioFrame(data) { // ArrayBuffer or Uint8Array
  if (!_decoder || _decoder.state !== 'configured') return;
  _decoder.decode(new EncodedAudioChunk({
    type: 'key', timestamp: _audioFrameTs, data,
  }));
  _audioFrameTs += AUDIO_BLOCK * 1e6 / AUDIO_SR;
}

// Called when audio_init arrives (WT bidi stream or as a protocol message)
function onAudioInit(b64streaminfo) {
  if (!_audioActive) return;
  const si = Uint8Array.from(atob(b64streaminfo), c => c.charCodeAt(0));
  setupDecoder(si);
  // Datagram reader is started in connect() once WT is live
}

function startAudioWs() {
  if (_audioWs) _audioWs.close();
  const ws = new WebSocket(`ws://${location.hostname}:4000/audio-ws`);
  ws.binaryType = 'arraybuffer';
  _audioWs = ws;
  let gotHeader = false;
  ws.onmessage = e => {
    if (!(e.data instanceof ArrayBuffer)) return;
    if (!gotHeader) {
      // First message: 42-byte stream header; STREAMINFO payload is bytes [8..42]
      setupDecoder(new Uint8Array(e.data, 8, 34));
      gotHeader = true;
    } else {
      if (_audioActive) feedAudioFrame(e.data);
    }
  };
  ws.onerror = ws.onclose = () => {
    if (_audioActive && !_wtObj) setTimeout(startAudioWs, 3000);
  };
}

function updateAudioBtns() {
  const label = _audioActive ? '\u23F9 Monitor' : '\uD83D\uDD0A Monitor'; // ⏹ / 🔊
  const col   = _audioActive ? '#70ffaa' : '#777';
  for (const id of ['audio-btn', 'sd-audio']) {
    const el = document.getElementById(id);
    if (el) { el.textContent = label; el.style.color = col; }
  }
}

// ── Transport ──────────────────────────────────────────────────────────────
async function connect() {
  doSend = null;
  const statusEl = document.getElementById('status');

  if (window.WebTransport) {
    statusEl.textContent = `WT ${window.location.hostname}:4433\u2026`;
    try {
      const transport = new WebTransport(`https://${window.location.hostname}:4433`, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: CERT_HASH }]
      });
      await transport.ready;
      statusEl.textContent = 'WebTransport';
      _wtObj = transport;

      // Datagram reader — runs concurrently, feeds audio frames when monitoring is on
      (async () => {
        try {
          const dtReader = transport.datagrams.readable.getReader();
          while (true) {
            const { value, done } = await dtReader.read();
            if (done) break;
            if (_audioActive) feedAudioFrame(value.buffer ?? value);
          }
        } catch(_) {}
        _wtObj = null;
      })();

      const bidiReader = transport.incomingBidirectionalStreams.getReader();
      const { value: bidi } = await bidiReader.read();
      const writer = bidi.writable.getWriter();
      doSend = obj => writer.write(enc.encode(JSON.stringify(obj) + '\n'));
      const stateReader = bidi.readable.getReader();
      let buf = '';
      while (true) {
        const { value, done } = await stateReader.read();
        if (done) break;
        buf += dec.decode(value, { stream: true });
        const lines = buf.split('\n');
        buf = lines.pop();
        for (const line of lines) {
          if (!line.trim()) continue;
          try {
            const json = JSON.parse(line);
            if (json.type === 'audio_init') { onAudioInit(json.streaminfo); }
            else { renderState(json); }
          } catch (_) {}
        }
      }
      doSend = null; _wtObj = null;
      statusEl.textContent = 'Reconnecting\u2026';
      setTimeout(connect, 3000);
      return;
    } catch (_) { doSend = null; _wtObj = null; }
  }

  await new Promise(resolve => {
    const wsUrl = `ws://${window.location.hostname}:4000/ws`;
    statusEl.textContent = 'WS\u2026';
    const ws = new WebSocket(wsUrl);
    ws.onopen    = () => { statusEl.textContent = 'WebSocket'; doSend = obj => ws.send(JSON.stringify(obj)); };
    ws.onmessage = e => { try { renderState(JSON.parse(e.data)); } catch (_) {} };
    ws.onerror   = () => { doSend = null; };
    ws.onclose   = () => { doSend = null; resolve(); };
  });

  statusEl.textContent = 'Reconnecting\u2026';
  setTimeout(connect, 3000);
}

connect();
</script>

<div id="qr-modal" onclick="this.style.display='none'" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,.92);z-index:9999;align-items:center;justify-content:center;flex-direction:column;gap:14px;">
  <img src="/qr" style="width:min(75vw,75vh);height:min(75vw,75vh);background:#fff;padding:14px;border-radius:6px;display:block;">
  <span style="color:#666;font-size:12px;user-select:none;">Tap anywhere to close</span>
</div>
</body>
</html>
"#;
