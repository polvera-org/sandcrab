// Safety = refusals, not warnings. Left: a crab heading from its jar to the shared
// pool is stopped at the barrier (no silent downgrade). Centre: the workspace
// sits under a shield; a symlink / rename-swap token bounces off it (fd-relative
// openat2, PathEscape); then an overlapping root tries to settle over it and is
// refused. Front: three review rounds light up, the last one PASS.
// One idea: "unsafe requests fail loudly; three review rounds made sure."
// Phases (15 s): downgrade 3.5 → symlink 3 → overlap 3 → reviews 3 → hold 2.5.
// Motion budget: one actor per phase, the shield flash, slow sway.
import { THREE, createKit, PALETTE as C, ease, win, clamp01, lerp, timeline, loopLength } from '../kit.js';
import { createCrab } from '../crab.js';
import { createJarBody } from '../jar.js';
import { anchor, label, crate, miniPool, ghostBox, loopU } from './common.js';

const PHASES = [
  { name: 'downgrade', d: 3.5 }, { name: 'symlink', d: 3 }, { name: 'overlap', d: 3 },
  { name: 'reviews', d: 3 }, { name: 'hold', d: 2.5 },
];

export const safetyMeta = {
  title: 'Safety',
  fallback: 'window-door',
  alt: 'A crab stopped at a barrier between jar and pool; a symlink bouncing off a shielded workspace; three review tokens lighting up',
  loop: loopLength(PHASES),
  still: 13.2,
  phases: PHASES,
  view: { target: [0, 0.55, 0.15], radius: 3.1, yaw: 0.34, pitch: 0.52, fraction: 0.9, sway: 0.03 },
};

export function createSafety() {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(6.8, 3.8, 0.3));

  // ── no silent downgrade (left) ──
  const jar = createJarBody({ scale: 0.45, sealed: true, crab: { pose: 'idle' } });
  jar.object3d.position.set(-2.7, 0, -0.75);
  g.add(jar.object3d);
  const pool = miniPool(k, 0.5);
  pool.position.set(-1.0, 0, -1.15);
  g.add(pool);
  const barrier = new THREE.Group();
  const mInk = k.toon(C.ink), mStripe = k.toon(C.paper);
  for (let i = 0; i < 5; i++) {
    const s = k.mesh(new THREE.BoxGeometry(0.16, 0.16, 0.06), i % 2 ? mInk : mStripe);
    s.position.set(-0.32 + i * 0.16, 0.4, 0);
    barrier.add(s);
  }
  for (const x of [-0.36, 0.36]) {
    // legs end just inside the stripe bottom: running through the stripes they sat 5 mm
    // behind its faces and shared its top face (z-fighting)
    const leg = k.mesh(new THREE.BoxGeometry(0.05, 0.34, 0.05).translate(0, 0.17, 0), mInk);
    leg.position.x = x;
    barrier.add(leg);
  }
  barrier.position.set(-1.8, 0, -0.55);
  barrier.rotation.y = 0.9;
  g.add(barrier);
  const walker = createCrab({ scale: 0.2, pose: 'idle', detail: 'low' });
  g.add(walker.object3d);
  const flashMat = k.basic(C.teal500, { transparent: true, depthWrite: false });
  const bFlash = new THREE.Mesh(k.geo(new THREE.RingGeometry(0.85, 1, 32)), flashMat);
  bFlash.position.set(-1.8, 0.4, -0.55);
  bFlash.rotation.y = 0.9;
  g.add(bFlash);

  // ── shielded workspace (centre-right) ──
  const ws = new THREE.Group();
  // lifted so the pad's top face is not coplanar with the slab top (z-fighting)
  ws.position.set(1.0, 0.02, -0.35);
  g.add(ws);
  const pad = k.disc(0.85, 0.08, C.sand50);
  ws.add(pad);
  [[-0.22, -0.12], [0.2, -0.15], [0.0, 0.22]].forEach(([x, z], i) => {
    const c = crate(k, 0.3, 0.15);
    c.mesh.position.set(x, i === 2 ? 0 : 0, z);
    ws.add(c.mesh);
  });
  const top = crate(k, 0.26, 0.15);
  top.mesh.position.set(-0.02, 0.3, -0.13);
  ws.add(top.mesh);
  const shieldGeo = k.geo(new THREE.SphereGeometry(0.95, 32, 16, 0, Math.PI * 2, 0, Math.PI / 2));
  const shieldMat = k.glass({ tint: C.teal500, opacity: 0.1, rimOpacity: 0.7 });
  const shield = new THREE.Mesh(shieldGeo, shieldMat);
  shield.renderOrder = 2;
  ws.add(shield);
  // the attacker: a symlink token (two interlocked ink links)
  const link = new THREE.Group();
  const lGeo = new THREE.TorusGeometry(0.1, 0.03, 6, 16);
  const l1 = k.mesh(lGeo, mInk, { outline: false });
  const l2 = k.mesh(lGeo, k.toon(C.teal700), { outline: false });
  l1.position.x = -0.08; l2.position.x = 0.08; l2.rotation.x = Math.PI / 2;
  link.add(l1, l2);
  g.add(link);
  // the overlapping root: a dashed box that tries to settle over the workspace
  const outer = ghostBox(k, 2.2, 1.3, 2.0, { opacity: 0.8 });
  g.add(outer);

  // ── review rounds (front) ──
  const tokens = [];
  for (let i = 0; i < 3; i++) {
    const m = k.toon(C.paper, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0 });
    const tk = k.mesh(new THREE.CylinderGeometry(0.22, 0.22, 0.1, 24).translate(0, 0.05, 0), m);
    tk.position.set(-0.75 + i * 0.75, 0.005, 1.35);
    g.add(tk);
    tokens.push({ tk, m });
  }
  const check = new THREE.Group();
  const mCheck = k.toon(C.paper);
  const c1 = k.mesh(new THREE.BoxGeometry(0.06, 0.03, 0.14), mCheck, { outline: false });
  c1.position.set(-0.05, 0, 0.03); c1.rotation.y = 0.8;
  const c2 = k.mesh(new THREE.BoxGeometry(0.06, 0.03, 0.26), mCheck, { outline: false });
  c2.position.set(0.06, 0, -0.02); c2.rotation.y = -0.6;
  check.add(c1, c2);
  check.position.set(0.75, 0.12, 1.35);
  g.add(check);

  const labels = [
    label('no silent downgrade: jar → pool refused', anchor(g, -2.1, 1.25, -0.8)),
    label('symlink or rename swap → refused (fd-relative)', anchor(g, 2.3, 0.05, 1.0), { below: true }),
    label('overlapping roots → refused', anchor(g, 1.0, 1.15, -0.35)),
    label('3 review rounds → PASS', anchor(g, 0, 0.45, 1.5)),
  ];

  const lerpV = (a, b, p) => [lerp(a[0], b[0], p), lerp(a[1], b[1], p), lerp(a[2], b[2], p)];

  function update(t, dt) {
    const u = loopU(t, safetyMeta.loop);
    const tl = timeline(PHASES, u);
    jar.update(t, dt);

    // walker: leaves the jar, walks to the barrier, is stopped, walks back
    const A = [-2.35, 0, -0.45], B = [-1.95, 0, -0.45];
    let wp = 0, walk = 0;
    if (tl.phase === 'downgrade') {
      const p = tl.p * 3.5;
      if (p < 1.4) { wp = ease(p / 1.4); walk = 1; }
      else if (p < 2.1) wp = 1;
      else if (p < 3.3) { wp = 1 - ease((p - 2.1) / 1.2); walk = 1; }
    }
    walker.object3d.position.set(...lerpV(A, B, wp));
    walker.object3d.rotation.y = 0.3; // crabs walk sideways
    walker.object3d.visible = tl.phase === 'downgrade';
    walker.setWalk(walk * 0.8);
    walker.update(t, dt);
    const fb = tl.phase === 'downgrade' ? clamp01((tl.p * 3.5 - 1.4) / 0.7) : -1;
    bFlash.visible = fb >= 0 && fb < 1;
    bFlash.scale.setScalar(0.25 + 0.35 * ease(Math.max(0, fb)));
    flashMat.opacity = 0.8 * (1 - Math.max(0, fb));

    // symlink token: flies in from the front-right, hits the shield, bounces off and fades
    let shieldHit = 0;
    link.visible = tl.phase === 'symlink';
    if (link.visible) {
      const p = tl.p;
      const hitP = 0.45;
      const S = [3.0, 0.9, 1.3], H = [1.85, 0.55, 0.3], E = [2.9, 1.6, 0.4];
      const pos = p < hitP ? lerpV(S, H, ease(p / hitP)) : lerpV(H, E, ease((p - hitP) / (1 - hitP)));
      link.position.set(...pos);
      link.rotation.set(p * 6, p * 4, 0);
      link.scale.setScalar(p < 0.85 ? 1 : 1 - (p - 0.85) / 0.15);
      if (p >= hitP) shieldHit = Math.exp(-(p - hitP) * 3 * 3);
    }
    // overlapping root: descends, stops above the shield, retreats
    outer.visible = tl.phase === 'overlap';
    if (outer.visible) {
      const p = tl.p;
      const d = p < 0.45 ? ease(p / 0.45) : p < 0.6 ? 1 : 1 - ease((p - 0.6) / 0.4);
      outer.position.set(1.0, lerp(2.2, 1.0, d), -0.35);
      if (p >= 0.45 && p < 0.6) shieldHit = Math.max(shieldHit, 1 - (p - 0.45) / 0.15);
    }
    shieldMat.uniforms.uOpacity.value = 0.1 + 0.25 * shieldHit;
    shieldMat.uniforms.uRimOpacity.value = 0.7 + 0.3 * shieldHit;

    // review tokens light up in sequence; round 3 gets the check
    const rp = tl.phase === 'reviews' ? tl.p * 3 : tl.phase === 'hold' ? 3 : 0;
    tokens.forEach((o, i) => {
      const on = clamp01(rp - i);
      o.m.color.set(on > 0.5 ? C.teal500 : C.paper);
      o.m.emissiveIntensity = on > 0 ? 0.15 + 0.5 * Math.exp(-(rp - i) * 2) : 0;
      o.tk.position.y = 0.005 + 0.06 * Math.sin(Math.PI * Math.min(1, on));
    });
    check.visible = rp >= 2.5;

    const focus = ['downgrade', 'symlink', 'overlap', 'reviews'];
    const idx = focus.indexOf(tl.phase);
    labels.forEach((l, i) => {
      if (tl.phase === 'hold') { l.a = i === 3 ? 1 : 0.45; return; }
      l.a = i === idx ? win(u, tl.from, tl.from + 0.4) : i < idx ? 0.45 : 0;
    });
  }

  return { object3d: g, meta: safetyMeta, labels, update, dispose() { jar.dispose(); walker.dispose(); k.dispose(); } };
}
