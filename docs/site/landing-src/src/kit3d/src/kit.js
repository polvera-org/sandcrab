// Shared building blocks for every Sandcrab scene: palette, easing, toon + outline
// materials, a per-scene resource tracker (so dispose() frees everything), and the
// sand slab / disc bases the illustrations stand on.
import * as THREE from '../vendor/three/0.186.1/three.module.min.js';

export { THREE };

/** Sandcrab palette. Coral is reserved for the crab. */
export const PALETTE = {
  sand50: '#F6F5F0',  // landing copy: chalk-white shore (kit default #F5ECD9)
  sand200: '#ECEAE2', // kit default #E8D5B0
  sandDeep: '#C9C5B8', // kit default #CDB083
  teal500: '#1F7A8C',
  teal700: '#0F4C5C',
  ink: '#0B2F38',
  coral: '#FF6B4A',
  coralDeep: '#E8553A',
  paper: '#FFFCF6',
  husk: '#E6ECEA',
  rock: '#6F8E94',
};

// ── timing helpers (all scenes are closed-form functions of t → seamless loops) ──
export const clamp01 = (x) => Math.min(1, Math.max(0, x));
export const lerp = (a, b, x) => a + (b - a) * x;
export const ease = (x) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2);
/** eased 0→1 progress of t inside the window [a, b] */
export const win = (t, a, b) => ease(clamp01((t - a) / (b - a)));
/** 0→1→0 bump inside [a, b] */
export const bump = (t, a, b) => { const x = clamp01((t - a) / (b - a)); return Math.sin(Math.PI * x); };
export const TAU = Math.PI * 2;

/** Deterministic RNG (mulberry32) so every load looks the same. */
export function rng(seed = 1) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let r = Math.imul(s ^ (s >>> 15), 1 | s);
    r = (r + Math.imul(r ^ (r >>> 7), 61 | r)) ^ r;
    return ((r ^ (r >>> 14)) >>> 0) / 4294967296;
  };
}

/** Look up the active phase of a timeline [{name, d}] at loop-local time u. */
export function timeline(phases, u) {
  let a = 0;
  for (const ph of phases) {
    if (u < a + ph.d) return { phase: ph.name, from: a, to: a + ph.d, p: (u - a) / ph.d };
    a += ph.d;
  }
  const last = phases[phases.length - 1];
  return { phase: last.name, from: a - last.d, to: a, p: 1 };
}
export const loopLength = (phases) => phases.reduce((s, p) => s + p.d, 0);
export const phaseStarts = (phases) => { let a = 0; return phases.map((p) => { const s = a; a += p.d; return { name: p.name, t: s }; }); };

// ── toon gradient: three flat tones, like the vector illustrations ──
let gradient;
function toonGradient() {
  if (!gradient) {
    gradient = new THREE.DataTexture(new Uint8Array([150, 210, 255]), 3, 1, THREE.RedFormat);
    gradient.minFilter = gradient.magFilter = THREE.NearestFilter;
    gradient.generateMipmaps = false;
    gradient.needsUpdate = true;
  }
  return gradient;
}

// ── screen-space ink outline (inverted hull, constant pixel width) ──
const OUTLINE = { uThickness: { value: 1.6 }, uScale: { value: 1 }, uRes: { value: new THREE.Vector2(1, 1) } };
/** Global outline width in CSS px (0 hides outlines). */
export function setOutlineWidth(px) { OUTLINE.uThickness.value = px; }
export function getOutlineWidth() { return OUTLINE.uThickness.value; }
const _size = new THREE.Vector2();
function hullBeforeRender(renderer) {
  renderer.getDrawingBufferSize(_size);
  OUTLINE.uRes.value.copy(_size);
  OUTLINE.uScale.value = renderer.getPixelRatio();
}

function outlineMaterial(color = PALETTE.ink) {
  return new THREE.ShaderMaterial({
    uniforms: {
      uThickness: OUTLINE.uThickness, uScale: OUTLINE.uScale, uRes: OUTLINE.uRes,
      uColor: { value: new THREE.Color(color) }, uOpacity: { value: 1 },
    },
    vertexShader: /* glsl */ `
      attribute vec3 outlineNormal;
      uniform float uThickness; uniform float uScale; uniform vec2 uRes;
      void main() {
        vec4 clip = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
        vec3 vn = normalMatrix * outlineNormal;
        vec2 d = vec2(vn.x * projectionMatrix[0][0], vn.y * projectionMatrix[1][1]);
        float l = length(d);
        if (l > 1e-6) clip.xy += (d / l) * uThickness * uScale * 2.0 / uRes * clip.w;
        clip.z += 0.0004 * clip.w;
        gl_Position = clip;
      }`,
    fragmentShader: /* glsl */ `
      uniform vec3 uColor; uniform float uOpacity;
      void main() {
        if (uOpacity < 0.01) discard;
        gl_FragColor = vec4(uColor, uOpacity);
        #include <colorspace_fragment>
      }`,
    side: THREE.BackSide,
  });
}

/** Averaged (crease-free) normals so hulls of boxes stay closed at corners. */
function ensureOutlineNormals(geo) {
  if (geo.getAttribute('outlineNormal')) return;
  if (!geo.getAttribute('normal')) geo.computeVertexNormals();
  const pos = geo.getAttribute('position');
  const nor = geo.getAttribute('normal');
  const acc = new Map();
  const key = (i) => `${pos.getX(i).toFixed(4)},${pos.getY(i).toFixed(4)},${pos.getZ(i).toFixed(4)}`;
  for (let i = 0; i < pos.count; i++) {
    const k = key(i);
    const v = acc.get(k) || [0, 0, 0];
    v[0] += nor.getX(i); v[1] += nor.getY(i); v[2] += nor.getZ(i);
    acc.set(k, v);
  }
  const out = new Float32Array(pos.count * 3);
  for (let i = 0; i < pos.count; i++) {
    const v = acc.get(key(i));
    const l = Math.hypot(v[0], v[1], v[2]) || 1;
    out[i * 3] = v[0] / l; out[i * 3 + 1] = v[1] / l; out[i * 3 + 2] = v[2] / l;
  }
  geo.setAttribute('outlineNormal', new THREE.BufferAttribute(out, 3));
}

// ── glyph textures (tool cards, labels) drawn on a canvas, no image files ──
export function canvasTexture(size, draw) {
  const c = document.createElement('canvas');
  c.width = c.height = size;
  draw(c.getContext('2d'), size);
  const tex = new THREE.CanvasTexture(c);
  tex.colorSpace = THREE.SRGBColorSpace;
  tex.anisotropy = 4;
  return tex;
}

/**
 * Per-scene resource tracker. Everything created through it is freed by dispose().
 */
export function createKit() {
  const geos = new Set();
  const mats = new Set();
  const texs = new Set();
  let defaultOutline;
  const k = {
    geo(g) { geos.add(g); return g; },
    mat(m) { mats.add(m); return m; },
    tex(t) { texs.add(t); return t; },
    toon(color, extra = {}) {
      return k.mat(new THREE.MeshToonMaterial({ color, gradientMap: toonGradient(), ...extra }));
    },
    basic(color, extra = {}) { return k.mat(new THREE.MeshBasicMaterial({ color, ...extra })); },
    outline(color) { return k.mat(outlineMaterial(color)); },
    /** Frosted glass: tinted body, ink rim at grazing angles, one highlight streak. */
    glass({ tint = PALETTE.teal500, rim = PALETTE.ink, opacity = 0.16, rimOpacity = 0.9, back = false } = {}) {
      return k.mat(new THREE.ShaderMaterial({
        uniforms: {
          uTint: { value: new THREE.Color(tint) }, uRim: { value: new THREE.Color(rim) },
          uOpacity: { value: opacity }, uRimOpacity: { value: rimOpacity }, uHi: { value: back ? 0 : 0.75 },
        },
        vertexShader: /* glsl */ `
          varying vec3 vN;
          void main() { vN = normalize(normalMatrix * normal); gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }`,
        fragmentShader: /* glsl */ `
          uniform vec3 uTint; uniform vec3 uRim; uniform float uOpacity; uniform float uRimOpacity; uniform float uHi;
          varying vec3 vN;
          void main() {
            vec3 n = normalize(vN);
            float f = 1.0 - abs(n.z);
            float rim = smoothstep(0.86, 0.985, f);
            float hi = uHi * smoothstep(0.09, 0.0, abs(n.x + 0.6)) * (1.0 - rim);
            vec3 c = mix(mix(uTint, uRim, rim), vec3(1.0), hi);
            float a = max(mix(uOpacity, uRimOpacity, rim), hi * 0.65);
            gl_FragColor = vec4(c, a);
            #include <colorspace_fragment>
          }`,
        transparent: true, depthWrite: false, side: back ? THREE.BackSide : THREE.FrontSide,
      }));
    },
    /** Attach an ink outline hull to a mesh (shares its geometry). */
    addOutline(mesh, mat) {
      ensureOutlineNormals(mesh.geometry);
      const m = mat || (defaultOutline ||= k.outline());
      const hull = new THREE.Mesh(mesh.geometry, m);
      hull.onBeforeRender = hullBeforeRender;
      hull.raycast = () => {};
      hull.userData.isOutline = true;
      mesh.add(hull);
      return hull;
    },
    /** Mesh with shadow flags + optional outline. */
    mesh(geo, mat, { outline = true, outlineMat, cast = true, receive = false } = {}) {
      const m = new THREE.Mesh(k.geo(geo), mat);
      m.castShadow = cast; m.receiveShadow = receive;
      if (outline) k.addOutline(m, outlineMat);
      return m;
    },
    /** Sand slab, top face at y = 0 (the illustrations' isometric ground tile). */
    slab(w, d, h = 0.3, color = PALETTE.sand200) {
      const g = new THREE.BoxGeometry(w, h, d);
      g.translate(0, -h / 2, 0);
      return k.mesh(g, k.toon(color), { receive: true });
    },
    /** Round sand disc, top face at y = 0. */
    disc(r, h = 0.25, color = PALETTE.sand200) {
      const g = new THREE.CylinderGeometry(r, r, h, 48, 1);
      g.translate(0, -h / 2, 0);
      return k.mesh(g, k.toon(color), { receive: true });
    },
    dispose() {
      geos.forEach((g) => g.dispose());
      mats.forEach((m) => m.dispose());
      texs.forEach((t) => t.dispose());
      geos.clear(); mats.clear(); texs.clear();
    },
  };
  return k;
}

/**
 * Set opacity on a list of materials. Fills keep writing depth so an object's own
 * outline hull (drawn after it in the transparent pass) stays hidden behind it.
 */
export function fadeMaterials(mats, a) {
  const t = a < 0.999;
  for (const m of mats) {
    if (m.uniforms && m.uniforms.uOpacity) { m.uniforms.uOpacity.value = a; m.transparent = t; continue; }
    if (m.transparent !== t) { m.transparent = t; m.needsUpdate = true; }
    m.opacity = a;
  }
}
