// Shared helpers for the presentation scenes (docs/site/presentation.html).
// Scenes here follow the kit contract — factory(opts) → { object3d, update(t, dt),
// dispose(), meta } — and add one optional field the kit stage ignores:
//   labels: [{ text, at: THREE.Object3D, a: 0..1, cls? }]
// A host page (the presentation) projects each label's anchor to the screen and
// draws it as HTML, so text stays crisp, themed and reflowable. Scenes update
// `text` and `a` (opacity) in update(); anchors move with the scene graph.
import { THREE, PALETTE as C, ease, clamp01, TAU, canvasTexture } from '../kit.js';

export const loopU = (t, L) => ((t % L) + L) % L;

/** Invisible anchor for a label. */
export function anchor(parent, x, y, z) {
  const o = new THREE.Object3D();
  o.position.set(x, y, z);
  parent.add(o);
  return o;
}

export const label = (text, at, extra = {}) => ({ text, at, a: 1, ...extra });

/** Dashed ink outline box (a "TODO" ghost), base at y = 0. */
export function ghostBox(k, w, h, d, { color = C.ink, opacity = 0.55, dash = 0.1, fill = C.paper, fillOpacity = 0.22 } = {}) {
  // Edges are lifted a hair (the bottom ones would otherwise lie in the ground plane) and
  // drawn after the fill without depth writes, so dashes never z-fight the fill or ground.
  const g = new THREE.Group();
  const geo = k.geo(new THREE.EdgesGeometry(new THREE.BoxGeometry(w, h, d).translate(0, h / 2 + 0.006, 0)));
  const lines = new THREE.LineSegments(geo, k.mat(new THREE.LineDashedMaterial({
    color, dashSize: dash, gapSize: dash * 0.7, transparent: true, opacity, depthWrite: false,
  })));
  lines.computeLineDistances();
  lines.renderOrder = 1;
  g.add(lines);
  if (fillOpacity > 0) {
    const f = new THREE.Mesh(k.geo(new THREE.BoxGeometry(w, h, d).translate(0, h / 2, 0)),
      k.basic(fill, { transparent: true, opacity: fillOpacity, depthWrite: false }));
    g.add(f);
  }
  return g;
}

/** Thin ink cable along a curve. */
export function cable(k, points, r = 0.035, color = C.ink) {
  const curve = new THREE.CatmullRomCurve3(points.map((p) => new THREE.Vector3(...p)));
  const mesh = k.mesh(new THREE.TubeGeometry(curve, 40, r, 5), k.toon(color), { outline: false });
  return { curve, mesh };
}

/** A protocol frame: 4 ink header cubes (u32 BE length) + a paper JSON body. */
export function framePacket(k) {
  const g = new THREE.Group();
  const mHead = k.toon(C.ink);
  const hGeo = new THREE.BoxGeometry(0.075, 0.13, 0.11);
  for (let i = 0; i < 4; i++) {
    const h = k.mesh(hGeo, mHead, { outline: false });
    h.position.set(-0.34 + i * 0.085, 0, 0);
    g.add(h);
  }
  const body = k.mesh(new THREE.BoxGeometry(0.4, 0.15, 0.12), k.toon(C.paper));
  body.position.x = 0.05;
  g.add(body);
  const stripe = k.mesh(new THREE.BoxGeometry(0.3, 0.035, 0.125), k.toon(C.teal500), { outline: false });
  stripe.position.set(0.05, 0.035, 0);
  g.add(stripe);
  return g;
}

/** Small shared tide pool (water disc, rock rim). Top at y≈0.02. */
export function miniPool(k, r = 0.6) {
  const g = new THREE.Group();
  const water = k.mesh(new THREE.CircleGeometry(r, 28).rotateX(-Math.PI / 2), k.toon(C.teal500), { outline: false, cast: false, receive: true });
  water.position.y = 0.02;
  g.add(water);
  const rim = k.mesh(new THREE.TorusGeometry(r + 0.04, 0.07, 5, 28).rotateX(Math.PI / 2), k.toon(C.rock), { receive: true });
  rim.scale.y = 0.6;
  rim.position.y = 0.03;
  g.add(rim);
  return g;
}

/** Teal crate (a file), base at y = 0. Returns { mesh, mat }. */
export function crate(k, s = 0.36, glow = 0.12) {
  const mat = k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: glow });
  const mesh = k.mesh(new THREE.BoxGeometry(s, s, s).translate(0, s / 2, 0), mat);
  const band = k.mesh(new THREE.BoxGeometry(s + 0.02, s * 0.16, s + 0.02).translate(0, s / 2, 0), k.toon(C.teal700), { outline: false });
  mesh.add(band);
  return { mesh, mat };
}

/** Teal server tower with vents and an LED. Base at y = 0. */
export function serverTower(k, w = 0.7, h = 1.2, d = 0.8) {
  const g = new THREE.Group();
  g.add(k.mesh(new THREE.BoxGeometry(w, h, d).translate(0, h / 2, 0), k.toon(C.teal700)));
  const mVent = k.toon(C.teal500);
  for (let i = 0; i < 3; i++) {
    const v = k.mesh(new THREE.BoxGeometry(w * 0.66, 0.045, 0.02), mVent, { outline: false });
    v.position.set(0, h * 0.82 - i * 0.11, d / 2 + 0.005);
    g.add(v);
  }
  const ledMat = k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.2 });
  const led = k.mesh(new THREE.BoxGeometry(0.09, 0.09, 0.03), ledMat, { outline: false });
  led.position.set(-w * 0.3, h * 0.2, d / 2 + 0.01);
  g.add(led);
  return { group: g, ledMat };
}

/** Upright paper panel with text drawn on a canvas (screens, signs). */
export function textTexture(k, lines, { size = 256, bg = C.teal700, fg = C.paper, font = '600 44px "JetBrains Mono", ui-monospace, monospace' } = {}) {
  return k.tex(canvasTexture(size, (ctx, s) => {
    ctx.fillStyle = bg; ctx.fillRect(0, 0, s, s);
    ctx.fillStyle = fg; ctx.font = font; ctx.textAlign = 'center'; ctx.textBaseline = 'middle';
    const arr = Array.isArray(lines) ? lines : [lines];
    arr.forEach((l, i) => ctx.fillText(l, s / 2, s / 2 + (i - (arr.length - 1) / 2) * 52));
  }));
}

/** Smoothly place an object along a curve at s (0..1), with lift. */
const _p = new THREE.Vector3();
export function onCurve(obj, curve, s, lift = 0) {
  curve.getPointAt(clamp01(s), _p);
  obj.position.set(_p.x, _p.y + lift, _p.z);
}

/** Face an object (default +Z forward) toward a point on the XZ plane. */
export function faceToward(obj, x, z) {
  obj.rotation.y = Math.atan2(x - obj.position.x, z - obj.position.z);
}

export { ease, clamp01, TAU };
