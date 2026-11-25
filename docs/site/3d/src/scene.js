// Stage: renderer + isometric orthographic camera + lights + soft shadow catcher.
// Handles resize, DPR cap, prefers-reduced-motion (renders one still frame),
// pausing when offscreen / tab hidden, light/dark theme, and a static .webp
// fallback when WebGL is unavailable or the context is lost.
import { THREE, setOutlineWidth, TAU } from './kit.js';

/** Default tunables. Any of these can be passed as opts.params or via stage.set(). */
export const DEFAULT_PARAMS = {
  speed: 1,        // time multiplier
  yaw: null,       // camera yaw (rad); null = scene default
  pitch: null,     // camera pitch (rad); null = scene default
  fraction: null,  // subject size as share of the short side; null = scene default
  sway: null,      // camera yaw sway amplitude (rad), one sway per loop
  outline: 1.6,    // ink outline width, CSS px
  shadow: null,    // shadow opacity; null = theme default
  maxDpr: 2,       // devicePixelRatio cap
  paused: false,
};

export function hasWebGL() {
  try {
    const c = document.createElement('canvas');
    return !!(window.WebGL2RenderingContext && c.getContext('webgl2'));
  } catch { return false; }
}

const FALLBACK_BASE = new URL('../../assets/crab/', import.meta.url);

function showFallback(el, meta, opts) {
  const img = document.createElement('img');
  img.src = new URL(`${meta.fallback}.webp`, opts.fallbackBase || FALLBACK_BASE).href;
  img.alt = meta.alt || meta.title || '';
  img.decoding = 'async';
  img.style.cssText = 'display:block;width:100%;height:100%;object-fit:contain;';
  img.className = 'sandcrab-3d-fallback';
  el.appendChild(img);
  return img;
}

function isDark() {
  const t = document.documentElement.getAttribute('data-theme');
  if (t === 'dark') return true;
  if (t === 'light') return false;
  return matchMedia('(prefers-color-scheme: dark)').matches;
}

/**
 * Create a stage inside `el` and run `def` (a scene definition: {create, meta}).
 * @param {HTMLElement} el
 * @param {{create:Function, meta:object}} def
 * @param {object} [opts] { params, sceneOpts, fallbackBase, t0, theme: 'auto'|'light'|'dark' }
 */
export function createStage(el, def, opts = {}) {
  const meta = def.meta;
  const P = { ...DEFAULT_PARAMS, ...(opts.params || {}) };
  const reduce = matchMedia('(prefers-reduced-motion: reduce)');
  const stub = (img) => ({
    el, fallback: img, meta, params: P, webgl: false,
    set() {}, seek() {}, play() {}, pause() {}, render() {}, info: () => ({ triangles: 0 }),
    get time() { return 0; },
    dispose() { img.remove(); },
  });
  if (!hasWebGL()) return stub(showFallback(el, meta, opts));

  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: true, powerPreference: 'low-power' });
  } catch {
    return stub(showFallback(el, meta, opts));
  }
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.shadowMap.enabled = true;
  renderer.shadowMap.type = THREE.PCFShadowMap;
  renderer.setClearColor(0x000000, 0);
  const canvas = renderer.domElement;
  canvas.style.cssText = 'display:block;width:100%;height:100%;touch-action:pan-y;';
  canvas.setAttribute('role', 'img');
  canvas.setAttribute('aria-label', meta.alt || meta.title || 'Sandcrab 3D scene');
  if (getComputedStyle(el).position === 'static') el.style.position = 'relative';
  el.appendChild(canvas);

  const scene = new THREE.Scene();
  const camera = new THREE.OrthographicCamera(-1, 1, 1, -1, 0.1, 200);
  const hemi = new THREE.HemisphereLight('#FFF6E6', '#BFA77C', 1.5);
  const sun = new THREE.DirectionalLight('#FFFFFF', 2.2);
  sun.castShadow = true;
  sun.shadow.mapSize.set(1024, 1024);
  sun.shadow.radius = 3;
  sun.shadow.bias = -0.001;
  sun.shadow.normalBias = 0.04;
  scene.add(hemi, sun, sun.target);

  const catcherMat = new THREE.ShadowMaterial({ opacity: 0.14 });
  const catcher = new THREE.Mesh(new THREE.PlaneGeometry(60, 60), catcherMat);
  catcher.rotation.x = -Math.PI / 2;
  catcher.position.y = meta.groundY ?? -0.3;
  catcher.receiveShadow = true;
  scene.add(catcher);

  const mod = def.create(opts.sceneOpts || {});
  scene.add(mod.object3d);

  const target = new THREE.Vector3(...meta.view.target);
  let t = opts.t0 ?? 0;
  let raf = 0;
  let last = 0;
  let visible = true;
  let disposed = false;
  let w = 1, h = 1;

  const get = (key) => (P[key] ?? meta.view[key] ?? DEFAULT_PARAMS[key]);

  function applyTheme() {
    const dark = opts.theme === 'dark' || (opts.theme !== 'light' && isDark());
    catcherMat.opacity = P.shadow ?? (dark ? 0.32 : 0.14);
    hemi.intensity = dark ? 1.35 : 1.5;
  }

  function applyView(tt) {
    const loop = meta.loop || 12;
    const yaw = get('yaw') + (get('sway') ?? 0) * Math.sin(TAU * tt / loop);
    const pitch = get('pitch');
    const dist = 40;
    camera.position.set(
      target.x + dist * Math.cos(pitch) * Math.sin(yaw),
      target.y + dist * Math.sin(pitch),
      target.z + dist * Math.cos(pitch) * Math.cos(yaw),
    );
    camera.lookAt(target);
    const aspect = w / h;
    const r = meta.view.radius;
    const f = get('fraction') ?? 0.6;
    let halfH = r / f;
    if (aspect < 1) halfH = Math.max(halfH, r / (f * aspect));
    camera.top = halfH; camera.bottom = -halfH;
    camera.left = -halfH * aspect; camera.right = halfH * aspect;
    camera.updateProjectionMatrix();
    // key light from upper-left of the camera
    const ly = yaw - 0.8;
    sun.position.set(target.x + 12 * Math.sin(ly), target.y + 16, target.z + 12 * Math.cos(ly));
    sun.target.position.copy(target);
    const sc = sun.shadow.camera;
    const sr = r * 1.8;
    sc.left = -sr; sc.right = sr; sc.top = sr; sc.bottom = -sr; sc.near = 1; sc.far = 60;
    sc.updateProjectionMatrix();
  }

  function renderAt(tt, dt) {
    if (disposed) return;
    setOutlineWidth(P.outline);
    mod.update(tt, dt);
    applyView(tt);
    renderer.render(scene, camera);
  }

  function frame(now) {
    raf = requestAnimationFrame(frame);
    const dt = Math.min(0.05, Math.max(0, (now - last) / 1000));
    last = now;
    if (!P.paused) t += dt * P.speed;
    renderAt(t, P.paused ? 0 : dt * P.speed);
  }

  function sync() {
    const animate = visible && !document.hidden && !reduce.matches && !disposed;
    if (animate && !raf) { last = performance.now(); raf = requestAnimationFrame(frame); }
    if (!animate && raf) { cancelAnimationFrame(raf); raf = 0; }
    if (reduce.matches && !disposed) { t = meta.still ?? 0; renderAt(t, 0); }
  }

  function resize() {
    const rect = el.getBoundingClientRect();
    w = Math.max(1, Math.round(rect.width));
    h = Math.max(1, Math.round(rect.height));
    renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, P.maxDpr));
    renderer.setSize(w, h, false);
    if (!raf) renderAt(t, 0);
  }

  const ro = new ResizeObserver(resize);
  ro.observe(el);
  const io = new IntersectionObserver((entries) => {
    visible = entries[entries.length - 1].isIntersecting;
    sync();
  }, { rootMargin: '120px' });
  io.observe(el);
  const onVis = () => sync();
  document.addEventListener('visibilitychange', onVis);
  reduce.addEventListener('change', onVis);
  const darkMq = matchMedia('(prefers-color-scheme: dark)');
  const onTheme = () => { applyTheme(); if (!raf) renderAt(t, 0); };
  darkMq.addEventListener('change', onTheme);
  const mo = new MutationObserver(onTheme);
  mo.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });

  let fallbackImg = null;
  canvas.addEventListener('webglcontextlost', (e) => {
    e.preventDefault();
    canvas.style.display = 'none';
    if (!fallbackImg) fallbackImg = showFallback(el, meta, opts);
  });

  applyTheme();
  resize();
  sync();

  return {
    el, meta, renderer, scene, camera, module: mod, params: P, webgl: true,
    get time() { return t; },
    /** Update tunables (see DEFAULT_PARAMS). */
    set(p) {
      Object.assign(P, p);
      if ('maxDpr' in p) resize();
      applyTheme();
      if (!raf) renderAt(t, 0);
    },
    /** Jump to scene time (seconds). */
    seek(tt) { t = tt; renderAt(t, 0); },
    play() { P.paused = false; },
    pause() { P.paused = true; },
    render() { renderAt(t, 0); },
    info() { return { triangles: renderer.info.render.triangles, calls: renderer.info.render.calls }; },
    dispose() {
      disposed = true;
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      ro.disconnect(); io.disconnect(); mo.disconnect();
      document.removeEventListener('visibilitychange', onVis);
      reduce.removeEventListener('change', onVis);
      darkMq.removeEventListener('change', onTheme);
      mod.dispose();
      catcher.geometry.dispose(); catcherMat.dispose();
      renderer.dispose();
      canvas.remove();
      fallbackImg?.remove();
    },
  };
}
