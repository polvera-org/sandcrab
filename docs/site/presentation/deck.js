// Sandcrab presentation — deck controller (classic script, works from file://).
// Owns the slide index, captions, progress dots, keyboard / click / swipe
// navigation, theme toggle, data-bound numbers and the no-3D fallback. The 3D
// layer (world.js, an ES module) registers itself as window.__pres3d.
(function () {
  'use strict';
  var root = document.documentElement;
  var slides = Array.prototype.slice.call(document.querySelectorAll('.slide'));
  var N = slides.length;
  var dotsEl = document.getElementById('dots');
  var counter = document.getElementById('counter');
  var prevBtn = document.getElementById('prev');
  var nextBtn = document.getElementById('next');
  var stage = document.getElementById('stage');
  var idx = 0;
  var mode = 'boot'; // boot | 3d | fallback

  function clamp(i) { return Math.max(0, Math.min(N - 1, i)); }
  var fromHash = parseInt((location.hash || '').replace('#', ''), 10);
  if (fromHash >= 1 && fromHash <= N) idx = fromHash - 1;

  // ── numbers from presentation/data.js (generated, never hand-typed) ──
  var D = window.SANDCRAB_DATA;
  function r(x) { return Math.round(x); }
  if (D) {
    var b = D.bench, l = D.live;
    var V = {
      spawnRange: r(b.spawn_ready_range_ms[0]) + '–' + r(b.spawn_ready_range_ms[1]) + ' ms',
      tearRange: r(b.teardown_range_ms[0]) + '–' + r(b.teardown_range_ms[1]) + ' ms',
      firstCall: b.base_w0.first_call.p50_ms.toFixed(1) + ' ms',
      benchN: String(b.n),
      benchFile: b.file.split('/').pop(),
      benchCommit: b.commit,
      cells: String(b.lifecycle_cells),
      toolCalls: String(l.tool_calls),
      wall: l.wall_min + ' minutes',
      cost: '$' + l.cost_usd.toFixed(2),
      toolErrors: l.tool_errors === 0 ? 'zero' : String(l.tool_errors),
      turns: String(l.turns),
    };
    Array.prototype.forEach.call(document.querySelectorAll('[data-v]'), function (el) {
      var v = V[el.getAttribute('data-v')];
      if (v != null) el.textContent = v;
    });
  }

  // ── dots ──
  slides.forEach(function (s, i) {
    var bt = document.createElement('button');
    bt.type = 'button';
    bt.className = 'dot';
    bt.setAttribute('aria-label', 'Slide ' + (i + 1) + ': ' + (s.getAttribute('data-title') || ''));
    bt.addEventListener('click', function (e) { e.stopPropagation(); go(i); });
    dotsEl.appendChild(bt);
  });
  var dots = Array.prototype.slice.call(dotsEl.children);

  function render() {
    slides.forEach(function (s, j) {
      var on = j === idx;
      s.classList.toggle('active', on);
      s.setAttribute('aria-hidden', on ? 'false' : 'true');
      if ('inert' in s) s.inert = !on;
    });
    dots.forEach(function (d, j) {
      if (j === idx) d.setAttribute('aria-current', 'step'); else d.removeAttribute('aria-current');
    });
    counter.textContent = String(idx + 1).padStart(2, '0') + ' / ' + String(N).padStart(2, '0');
    prevBtn.disabled = idx === 0;
    nextBtn.disabled = idx === N - 1;
    if (mode === 'fallback') loadImg(slides[idx]);
  }

  function go(i, opts) {
    i = clamp(i);
    if (i === idx) return;
    idx = i;
    render();
    try { history.replaceState(null, '', '#' + (i + 1)); } catch (e) { /* file:// may refuse */ }
    if (window.__pres3d) window.__pres3d.show(i, opts || {});
  }

  // ── keyboard ──
  var digits = '', digitTimer = 0;
  addEventListener('keydown', function (e) {
    var t = e.target;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (t && t.closest && t.closest('input, textarea, select, [contenteditable="true"]')) return;
    var k = e.key;
    if (k === 'ArrowRight' || k === 'PageDown' || (k === ' ' && !e.shiftKey)) { e.preventDefault(); go(idx + 1); }
    else if (k === 'ArrowLeft' || k === 'PageUp' || (k === ' ' && e.shiftKey)) { e.preventDefault(); go(idx - 1); }
    else if (k === 'Home') { e.preventDefault(); go(0); }
    else if (k === 'End') { e.preventDefault(); go(N - 1); }
    else if (/^[0-9]$/.test(k)) {
      // "1".."9" jump at once; a second digit within 600 ms makes 10, 11…; "0" alone = 10
      clearTimeout(digitTimer);
      digits += k;
      var n = parseInt(digits, 10);
      if (digits.length === 1 && k !== '0' && k !== '1') { go(n - 1); digits = ''; return; }
      if (digits.length >= 2) { go((n >= 1 && n <= N ? n : parseInt(k, 10) || 10) - 1); digits = ''; return; }
      digitTimer = setTimeout(function () { go((k === '0' ? 10 : n) - 1); digits = ''; }, 600);
    }
  });

  // ── click / swipe on the stage ──
  prevBtn.addEventListener('click', function () { go(idx - 1); });
  nextBtn.addEventListener('click', function () { go(idx + 1); });
  var downX = null, downY = null;
  stage.addEventListener('pointerdown', function (e) { downX = e.clientX; downY = e.clientY; });
  stage.addEventListener('pointerup', function (e) {
    if (downX == null) return;
    var dx = e.clientX - downX, dy = e.clientY - downY;
    downX = null;
    if (Math.abs(dx) > 48 && Math.abs(dx) > Math.abs(dy)) { go(idx + (dx < 0 ? 1 : -1)); return; }
    if (Math.abs(dx) < 8 && Math.abs(dy) < 8) go(idx + (e.clientX < innerWidth / 3 ? -1 : 1));
  });

  // ── theme toggle (auto → light → dark), shared key with the landing page ──
  var themeBtn = document.getElementById('theme');
  var THEMES = ['auto', 'light', 'dark'];
  function applyTheme(t) {
    if (t === 'light' || t === 'dark') root.setAttribute('data-theme', t); else root.removeAttribute('data-theme');
    themeBtn.textContent = t.charAt(0).toUpperCase() + t.slice(1);
    themeBtn.setAttribute('aria-label', 'Theme: ' + t + '. Change theme');
  }
  var theme = 'auto';
  try { theme = localStorage.getItem('sandcrab-theme') || 'auto'; } catch (e) { /* ignore */ }
  if (THEMES.indexOf(theme) < 0) theme = 'auto';
  applyTheme(theme);
  themeBtn.addEventListener('click', function () {
    theme = THEMES[(THEMES.indexOf(theme) + 1) % THEMES.length];
    try { localStorage.setItem('sandcrab-theme', theme); } catch (e) { /* ignore */ }
    applyTheme(theme);
  });

  // ── fallback: symbology illustrations instead of 3D ──
  function loadImg(s) {
    var img = s.querySelector('img.fb');
    if (img && !img.getAttribute('src')) img.setAttribute('src', img.getAttribute('data-src'));
  }
  function fallback(reason) {
    if (mode === 'fallback') return;
    mode = 'fallback';
    root.classList.add('no3d');
    root.classList.remove('has3d');
    var hint = document.getElementById('hint');
    if (location.protocol === 'file:' && reason !== 'nowebgl') hint.hidden = false;
    else if (reason === 'nowebgl') { hint.hidden = false; hint.querySelector('.why').textContent = 'WebGL is not available in this browser, so the slides show the illustrations instead of the 3D scenes.'; }
    loadImg(slides[idx]);
    var next = slides[idx + 1];
    if (next) loadImg(next);
  }

  window.__deck = {
    get index() { return idx; },
    count: N,
    go: go,
    slides: slides.map(function (s) { return { scene: s.getAttribute('data-scene'), title: s.getAttribute('data-title') }; }),
    fallback: fallback,
    ready3d: function () { if (mode !== 'fallback') { mode = '3d'; root.classList.add('has3d'); } },
  };

  render();
  // If the module never boots (file:// blocks ES modules), fall back after load.
  addEventListener('load', function () {
    setTimeout(function () { if (!window.__pres3dBooted) fallback(location.protocol === 'file:' ? 'file' : 'module'); }, 1200);
  });
})();
