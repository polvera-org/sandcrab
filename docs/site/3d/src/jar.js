// Sealed jar = hardware isolation. Each sandbox is a crab in its own microVM jar.
// One idea: "every crab gets its own sealed jar."
// Phases (12 s): open 2.5 → seal 1.8 (lid descends + screws down) → click 0.6 (glint)
//   → sealed 4.6 (crab idles inside) → reset 2.5 (lid lifts back).
// Motion budget: crab idle (always), lid (seal/reset only), glint (click only), slow camera sway.
import { THREE, createKit, PALETTE as C, win, bump, loopLength, timeline } from './kit.js';
import { createCrab } from './crab.js';

const PHASES = [
  { name: 'open', d: 2.5 }, { name: 'seal', d: 1.8 }, { name: 'click', d: 0.6 },
  { name: 'sealed', d: 4.6 }, { name: 'reset', d: 2.5 },
];

export const jarMeta = {
  title: 'Sealed jar',
  fallback: 'isolation',
  alt: 'A coral crab sealed inside its own glass jar',
  loop: loopLength(PHASES),
  still: 7,
  phases: PHASES,
  view: { target: [0, 1.15, 0], radius: 1.6, yaw: 0.5, pitch: 0.48, fraction: 0.72, sway: 0.06 },
};

const PROFILE = [
  [0, 0], [0.86, 0], [0.98, 0.06], [1.02, 0.2], [1.02, 1.55], [0.96, 1.72], [0.84, 1.82], [0.8, 1.86], [0.8, 2.02],
];

/**
 * Glass jar with a crab inside (no ground). Reused by wire.js.
 * @param {object} [opts] { scale=1, sealed=false (lid always closed), crab: crab opts }
 */
export function createJarBody(opts = {}) {
  const { scale = 1, sealed = false } = opts;
  const k = createKit();
  const g = new THREE.Group();
  g.scale.setScalar(scale);

  // sand bed + crab
  const bed = k.mesh(new THREE.CylinderGeometry(0.9, 0.94, 0.14, 24).translate(0, 0.07, 0), k.toon(C.sand200), { outline: false, receive: true });
  g.add(bed);
  const crab = createCrab({ scale: 0.44, pose: 'idle', detail: scale < 0.9 ? 'low' : 'high', ...(opts.crab || {}) });
  crab.object3d.position.y = 0.14;
  g.add(crab.object3d);

  // glass: back faces first, then front faces (both transparent, no depth write)
  const pts = PROFILE.map(([x, y]) => new THREE.Vector2(x, y));
  const glassGeo = k.geo(new THREE.LatheGeometry(pts, 28));
  const back = new THREE.Mesh(glassGeo, k.glass({ back: true, opacity: 0.08, rimOpacity: 0.25 }));
  back.renderOrder = 1;
  const front = new THREE.Mesh(glassGeo, k.glass({ opacity: 0.12, rimOpacity: 0.95 }));
  front.renderOrder = 2;
  g.add(back, front);
  // rim rings so the mouth and base read as solid ink lines
  const ringGeo = k.geo(new THREE.TorusGeometry(0.8, 0.022, 4, 28).rotateX(Math.PI / 2));
  const mouth = new THREE.Mesh(ringGeo, k.basic(C.ink));
  mouth.position.y = 2.02;
  g.add(mouth);

  // teal screw lid with thread ridges
  const lid = new THREE.Group();
  const mLid = k.toon(C.teal500);
  const cap = k.mesh(new THREE.CylinderGeometry(0.88, 0.88, 0.3, 28), mLid);
  cap.position.y = 0.15;
  lid.add(cap);
  const ridgeGeo = new THREE.TorusGeometry(0.89, 0.025, 3, 28).rotateX(Math.PI / 2);
  const mRidge = k.toon(C.teal700);
  for (let i = 0; i < 3; i++) {
    const ridge = k.mesh(ridgeGeo, mRidge, { outline: false });
    ridge.position.y = 0.06 + i * 0.09;
    lid.add(ridge);
  }
  const knurl = k.mesh(new THREE.BoxGeometry(0.1, 0.05, 0.3), k.toon(C.teal700), { outline: false });
  knurl.position.set(0, 0.31, 0.45);
  lid.add(knurl);
  g.add(lid);

  // click glint: a 4-point star that always faces the camera
  const starShape = new THREE.Shape();
  for (let i = 0; i < 8; i++) {
    const a = (i / 8) * Math.PI * 2 + Math.PI / 2;
    const r = i % 2 ? 0.05 : 0.22;
    const x = Math.cos(a) * r, y = Math.sin(a) * r;
    if (i === 0) starShape.moveTo(x, y); else starShape.lineTo(x, y);
  }
  const star = new THREE.Mesh(k.geo(new THREE.ShapeGeometry(starShape)), k.basic('#FFFFFF', { transparent: true, depthWrite: false }));
  star.renderOrder = 3;
  star.onBeforeRender = (r, s, cam) => { star.quaternion.copy(cam.quaternion); };
  g.add(star);

  function setLid(open) {
    // open 1 → hovering above, quarter-turned; 0 → screwed down on the mouth
    lid.position.y = 1.86 + open * 0.6;
    lid.rotation.y = open * Math.PI * 1.5;
  }

  function update(t, dt) {
    crab.update(t, dt);
    if (sealed) { setLid(0); star.visible = false; return; }
    const u = ((t % jarMeta.loop) + jarMeta.loop) % jarMeta.loop;
    const tl = timeline(PHASES, u);
    let open = 1;
    if (tl.phase === 'seal') open = 1 - win(u, tl.from, tl.to);
    else if (tl.phase === 'click' || tl.phase === 'sealed') open = 0;
    else if (tl.phase === 'reset') open = win(u, tl.from, tl.to);
    setLid(open);
    const s = tl.phase === 'click' ? bump(u, tl.from, tl.to) : 0;
    star.visible = s > 0.01;
    star.scale.setScalar(0.2 + s);
    star.material.opacity = s;
    star.position.set(0.62, 2.25, 0.55);
    // crab wanders a little inside: small sideways shuffle during the sealed hold
    const shuffle = Math.sin((Math.PI * 2 * u) / 6);
    crab.object3d.position.x = 0.12 * shuffle;
    crab.setWalk(Math.abs(Math.cos((Math.PI * 2 * u) / 6)) * 0.6);
  }

  return { object3d: g, crab, update, dispose() { crab.dispose(); k.dispose(); } };
}

/** Jar scene: jar on a sand disc. opts: { crab } */
export function createJar(opts = {}) {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.disc(1.75, 0.25));
  const jar = createJarBody(opts);
  g.add(jar.object3d);
  return {
    object3d: g, meta: jarMeta, jar,
    update: (t, dt) => jar.update(t, dt),
    dispose() { jar.dispose(); k.dispose(); },
  };
}
