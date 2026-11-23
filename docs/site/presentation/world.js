// Sandcrab presentation — 3D layer.
// One renderer, one THREE.Scene. Every slide's kit scene sits at its own spot on a
// long shore (x = i * SPACING); only the current slide (and, during a move, the one
// we are leaving) is visible and updated. Moving between slides is one camera
// motion: an eased pan with a small zoom-out, ~1.5 s. Captions, dots and keys live
// in deck.js (classic script); this module only renders and reports readiness.
import { THREE, setOutlineWidth, ease, clamp01, lerp, TAU } from '../3d/src/kit.js';
import { hasWebGL } from '../3d/src/scene.js';
import { PRES_SCENES } from '../3d/src/scenes/index.js';

const deck = window.__deck;
const SPACING = 10; // neighbours are hidden at rest; during a move both share the frame
const TUNE_KEY = 'sandcrab-pres-tune';
const DEFAULTS = { speed: 1, transition: 1.5, zoom: 1, yaw: 0, pitch: 0, sway: 1, outline: 1.6, maxDpr: 2 };
const P = { ...DEFAULTS };
const tuning = new URLSearchParams(location.search).has('tune');
if (tuning) { try { Object.assign(P, JSON.parse(localStorage.getItem(TUNE_KEY) || '{}')); } catch { /* ignore */ } }

function boot() {
  const el = document.getElementById('stage');
  if (!deck || !el) return;
  if (!hasWebGL()) { deck.fallback('nowebgl'); return; }
  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true, powerPreference: 'high-performance' });
  } catch { deck.fallback('nowebgl'); return; }
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = THREE.PCFShadowMap;
  renderer.setClearColor(0x000000, 0);
  const canvas = renderer.domElement;
  canvas.setAttribute('role', 'img');
  el.appendChild(canvas);

  const scene = new THREE.Scene();
  const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 400);
  const hemi = new THREE.HemisphereLight('#FFF6E6', '#BFA77C', 1.5);
  const sun = new THREE.DirectionalLight('#FFFFFF', 2.2);
  sun.castShadow = true;
  sun.shadow.mapSize.set(1536, 1536);
  sun.shadow.radius = 3;
  sun.shadow.bias = -0.001;
  sun.shadow.normalBias = 0.04;
  scene.add(hemi, sun, sun.target);
  const catcherMat = new THREE.ShadowMaterial({ opacity: 0.14 });

  const labelLayer = document.getElementById('labels');
  const opts = { data: window.SANDCRAB_DATA, screenshot: new URL('landing-shot.webp', import.meta.url).href, onAsset: () => { if (!raf) render(); } };
  const slides = deck.slides.map((s, i) => {
    const def = PRES_SCENES[s.scene];
    const mod = def.create(opts);
    const meta = mod.meta || def.meta;
    const group = new THREE.Group();
    group.position.x = i * SPACING;
    group.add(mod.object3d);
    const catcher = new THREE.Mesh(new THREE.PlaneGeometry(16, 16), catcherMat);
    catcher.rotation.x = -Math.PI / 2;
    catcher.position.y = meta.groundY ?? -0.3;
    catcher.receiveShadow = true;
    group.add(catcher);
    group.visible = false;
    scene.add(group);
    const labels = (mod.labels || []).map((l) => {
      const n = document.createElement('div');
      n.className = `lbl ${l.cls || ''}`;
      n.textContent = l.text;
      n.style.display = 'none';
      labelLayer.appendChild(n);
      return { l, n, text: l.text };
    });
    return { i, mod, meta, group, catcher, labels, t: 0 };
  });

  const reduce = matchMedia('(prefers-reduced-motion: reduce)');
  let cur = deck.index, from = -1, transStart = 0, raf = 0, last = 0, paused = false;
  let virt = null; // long jumps start from a virtual view half a slide away, not across the whole shore
  const WIDE = 1.2; // scenes are wider than tall: the region's width must hold WIDE × span
  let w = 1, h = 1;
  const stats = { frames: 0, frameMs: 0, renderMs: 0 };

  // ── camera framing ────────────────────────────────────────────────
  // The subject is fitted into the free region next to (desktop) or above (phone)
  // the caption, by shifting the orthographic frustum, not the camera.
  function region() {
    const cap = document.querySelector('.slide.active .cap');
    const top = 64, bottom = 64;
    if (innerWidth >= 900 && cap) {
      const x0 = cap.offsetLeft + cap.offsetWidth + 24;
      return { x0, y0: top, x1: w - 24, y1: h - bottom };
    }
    const capTop = cap ? h - (cap.offsetHeight + 84) : h * 0.6;
    return { x0: 8, y0: top - 8, x1: w - 8, y1: Math.max(top + 160, capTop) };
  }
  function viewOf(s) {
    const v = s.meta.view;
    const sway = (v.sway ?? 0) * P.sway * Math.sin((TAU * s.t) / (s.meta.loop || 12));
    return {
      tx: v.target[0] + s.i * SPACING, ty: v.target[1], tz: v.target[2],
      yaw: v.yaw + P.yaw + sway, pitch: v.pitch + P.pitch,
      span: v.radius / ((v.fraction ?? 0.8) * P.zoom), // world half-extent the region must show
    };
  }
  const tmpT = new THREE.Vector3();
  function applyCamera(now) {
    const R = region();
    const rw = Math.max(1, R.x1 - R.x0), rh = Math.max(1, R.y1 - R.y0);
    const cx = (R.x0 + R.x1) / 2, cy = (R.y0 + R.y1) / 2;
    let V = viewOf(slides[cur]);
    let bumpK = 0;
    if (from >= 0 || virt) {
      const p = clamp01((now - transStart) / (P.transition * 1000));
      const e = ease(p);
      const A = virt || viewOf(slides[from]);
      V = {
        tx: lerp(A.tx, V.tx, e), ty: lerp(A.ty, V.ty, e), tz: lerp(A.tz, V.tz, e),
        yaw: lerp(A.yaw, V.yaw, e), pitch: lerp(A.pitch, V.pitch, e), span: lerp(A.span, V.span, e),
      };
      bumpK = Math.sin(Math.PI * p) * (virt ? 0.15 : 0.18);
    }
    const s = Math.max(V.span / (rh / 2), (V.span * WIDE) / (rw / 2)) * (1 + bumpK); // world units per px
    const halfW = (w / 2) * s, halfH = (h / 2) * s;
    const dx = (cx - w / 2) * s, dy = (h / 2 - cy) * s;
    camera.left = -halfW - dx; camera.right = halfW - dx;
    camera.top = halfH - dy; camera.bottom = -halfH - dy;
    camera.updateProjectionMatrix();
    tmpT.set(V.tx, V.ty, V.tz);
    const dist = 60;
    camera.position.set(
      V.tx + dist * Math.cos(V.pitch) * Math.sin(V.yaw),
      V.ty + dist * Math.sin(V.pitch),
      V.tz + dist * Math.cos(V.pitch) * Math.cos(V.yaw),
    );
    camera.lookAt(tmpT);
    const ly = V.yaw - 0.8;
    sun.position.set(V.tx + 14 * Math.sin(ly), V.ty + 18, V.tz + 14 * Math.cos(ly));
    sun.target.position.copy(tmpT);
    const sr = V.span * 1.5;
    const sc = sun.shadow.camera;
    sc.left = -sr; sc.right = sr; sc.top = sr; sc.bottom = -sr; sc.near = 1; sc.far = 80;
    sc.updateProjectionMatrix();
  }

  // ── labels ────────────────────────────────────────────────────────
  const wp = new THREE.Vector3();
  function drawLabels(now) {
    const moving = from >= 0 || !!virt;
    const p = moving ? clamp01((now - transStart) / (P.transition * 1000)) : 1;
    for (const s of slides) {
      let k = 0;
      if (s.i === cur) k = moving ? clamp01((p - 0.6) / 0.4) : 1;
      else if (s.i === from) k = 1 - clamp01(p / 0.25);
      for (const o of s.labels) {
        const a = k * (o.l.a ?? 1);
        if (a < 0.02) { if (o.n.style.display !== 'none') o.n.style.display = 'none'; continue; }
        if (o.text !== o.l.text) { o.text = o.l.text; o.n.textContent = o.text; o.wid = 0; }
        o.l.at.getWorldPosition(wp).project(camera);
        if (o.n.style.display === 'none') { o.n.style.display = ''; o.wid = 0; }
        if (!o.wid) o.wid = o.n.offsetWidth;
        // keep labels on screen (phones): clamp the centre so the chip never clips
        const half = o.wid / 2 + 6;
        const x = Math.min(w - half, Math.max(half, (wp.x * 0.5 + 0.5) * w)), y = (-wp.y * 0.5 + 0.5) * h;
        o.n.style.opacity = a.toFixed(3);
        o.n.style.transform = `translate(${x.toFixed(1)}px, ${y.toFixed(1)}px) translate(-50%, ${o.l.below ? '6px' : 'calc(-100% - 6px)'})`;
      }
    }
  }

  // ── render loop ───────────────────────────────────────────────────
  function render(now = performance.now(), dt = 0) {
    const t0 = performance.now();
    setOutlineWidth(P.outline);
    if ((from >= 0 || virt) && now - transStart >= P.transition * 1000) { if (from >= 0) slides[from].group.visible = false; from = -1; virt = null; }
    for (const s of slides) {
      const vis = s.i === cur || s.i === from;
      s.group.visible = vis;
      if (vis) s.mod.update(s.t, dt);
    }
    applyCamera(now);
    renderer.render(scene, camera);
    drawLabels(now);
    stats.renderMs = stats.renderMs * 0.9 + (performance.now() - t0) * 0.1;
  }
  function frame(now) {
    raf = requestAnimationFrame(frame);
    const real = now - last;
    const dt = Math.min(0.05, Math.max(0, real / 1000)) * (paused ? 0 : P.speed);
    last = now;
    if (stats.frames++ > 2) stats.frameMs = stats.frameMs ? stats.frameMs * 0.9 + real * 0.1 : real;
    slides[cur].t += dt;
    if (from >= 0) slides[from].t += dt;
    render(now, dt);
  }
  function sync() {
    const animate = !document.hidden && !reduce.matches;
    if (animate && !raf) { last = performance.now(); raf = requestAnimationFrame(frame); }
    if (!animate && raf) { cancelAnimationFrame(raf); raf = 0; }
    if (reduce.matches) { from = -1; virt = null; slides[cur].t = slides[cur].meta.still ?? 0; render(); }
  }
  function resize() {
    const r = el.getBoundingClientRect();
    w = Math.max(1, Math.round(r.width)); h = Math.max(1, Math.round(r.height));
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, P.maxDpr));
    renderer.setSize(w, h, false);
    canvas.style.width = '100%'; canvas.style.height = '100%';
    if (!raf) render();
  }
  function applyTheme() {
    const t = document.documentElement.getAttribute('data-theme');
    const dark = t === 'dark' || (t !== 'light' && matchMedia('(prefers-color-scheme: dark)').matches);
    catcherMat.opacity = dark ? 0.3 : 0.14;
    hemi.intensity = dark ? 1.35 : 1.5;
    if (!raf) render();
  }

  /** Go to slide i. opts.instant skips the camera move. */
  function show(i, o = {}) {
    if (i === cur && from < 0 && !o.instant) return;
    const instant = o.instant || reduce.matches;
    const jump = i - cur;
    from = -1; virt = null;
    if (!instant && Math.abs(jump) === 1) { from = cur; transStart = performance.now(); }
    if (from >= 0) slides[from].group.visible = true;
    cur = i;
    slides[cur].t = reduce.matches ? (slides[cur].meta.still ?? 0) : 0;
    if (!instant && Math.abs(jump) > 1) {
      virt = viewOf(slides[cur]);
      virt.tx -= Math.sign(jump) * SPACING * 0.5;
      transStart = performance.now();
    }
    canvas.setAttribute('aria-label', slides[cur].meta.alt || slides[cur].meta.title);
    if (!raf) render();
  }

  new ResizeObserver(resize).observe(el);
  document.addEventListener('visibilitychange', sync);
  reduce.addEventListener('change', sync);
  matchMedia('(prefers-color-scheme: dark)').addEventListener('change', applyTheme);
  new MutationObserver(applyTheme).observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
  canvas.addEventListener('webglcontextlost', (e) => { e.preventDefault(); deck.fallback('contextlost'); });

  // compile every slide's shaders once so the first visit to a slide does not hitch
  slides.forEach((s) => { s.group.visible = true; });
  resize();
  try { renderer.compile(scene, camera); } catch { /* optional */ }
  slides.forEach((s) => { s.group.visible = false; });
  applyTheme();
  show(cur, { instant: true });
  sync();

  window.__pres3d = {
    show,
    /** Freeze the current slide at time t (default: its still frame) and render. */
    freeze(t) {
      paused = true; from = -1; virt = null;
      slides[cur].t = t ?? slides[cur].meta.still ?? 0;
      render(performance.now(), 0);
    },
    play() { paused = false; },
    stats: () => ({ ...stats, slide: cur, t: slides[cur].t, ...renderer.info.render }),
    params: P,
  };
  deck.ready3d();
  if (tuning) buildPanel();

  // ── tuning panel (?tune) ──────────────────────────────────────────
  function buildPanel() {
    const box = document.createElement('aside');
    box.className = 'tune custom-scrollbar';
    box.setAttribute('aria-label', 'Tuning panel');
    const rows = [
      ['speed', 0.25, 2, 0.05], ['transition', 0.6, 3, 0.1], ['zoom', 0.6, 1.4, 0.02],
      ['yaw', -0.6, 0.6, 0.01], ['pitch', -0.3, 0.3, 0.01], ['sway', 0, 2, 0.1],
      ['outline', 0, 3, 0.1], ['maxDpr', 1, 3, 0.25],
    ];
    box.innerHTML = '<h3>Tune</h3>' + rows.map(([k, a, b, st]) =>
      `<label>${k}<input type="range" min="${a}" max="${b}" step="${st}" value="${P[k]}" data-k="${k}"><output>${P[k]}</output></label>`).join('') +
      '<div class="row"><button class="btn" data-a="pause">Pause (p)</button><button class="btn" data-a="copy">Copy params</button><button class="btn" data-a="reset">Reset</button></div><div class="stat" aria-live="off"></div>';
    document.body.appendChild(box);
    const save = () => { try { localStorage.setItem(TUNE_KEY, JSON.stringify(P)); } catch { /* ignore */ } };
    box.addEventListener('input', (e) => {
      const k = e.target.dataset.k; if (!k) return;
      P[k] = parseFloat(e.target.value);
      e.target.nextElementSibling.textContent = P[k];
      if (k === 'maxDpr') resize();
      save(); if (!raf) render();
    });
    const pauseBtn = box.querySelector('[data-a=pause]');
    const togglePause = () => { paused = !paused; pauseBtn.setAttribute('aria-pressed', String(paused)); };
    box.addEventListener('click', (e) => {
      const a = e.target.dataset.a;
      if (a === 'pause') togglePause();
      if (a === 'copy') navigator.clipboard?.writeText(JSON.stringify(P, null, 1));
      if (a === 'reset') {
        Object.assign(P, DEFAULTS); save();
        box.querySelectorAll('input').forEach((i) => { i.value = P[i.dataset.k]; i.nextElementSibling.textContent = P[i.dataset.k]; });
        resize();
      }
    });
    addEventListener('keydown', (e) => { if (e.key === 'p' && !e.target.closest?.('input')) togglePause(); });
    const st = box.querySelector('.stat');
    setInterval(() => {
      const i = renderer.info.render;
      st.textContent = `frame ${stats.frameMs.toFixed(1)} ms · render ${stats.renderMs.toFixed(1)} ms · ${(i.triangles / 1000).toFixed(1)}k tris · ${i.calls} calls · slide ${cur + 1} t=${slides[cur].t.toFixed(1)}s`;
    }, 500);
  }
}

window.__pres3dBooted = true;
try { boot(); } catch (e) { console.error(e); deck?.fallback('error'); }
