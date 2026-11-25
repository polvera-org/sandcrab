// Shared tide pool = shared kernel (Docker). Several grey crabs jostle in one
// pool of water; a striped barrier marks it as a deliberate, explicit choice.
// One idea: "containers share one pool; entering it is opt-in."
// Phases (12 s): jostle 12 (continuous; one ripple every 3 s).
// Motion budget: crabs wander (closed-form, periods divide 12 s), ripple ring, slow sway.
import { THREE, createKit, PALETTE as C, rng, TAU, loopLength } from './kit.js';
import { createCrab } from './crab.js';

const PHASES = [{ name: 'jostle', d: 12 }];

export const tidepoolMeta = {
  title: 'Shared tide pool',
  fallback: 'isolation',
  alt: 'Grey crabs crowded together in one shared tide pool behind a barrier',
  loop: loopLength(PHASES),
  still: 2,
  phases: PHASES,
  view: { target: [0, 0.1, 0.1], radius: 2.4, yaw: 0.45, pitch: 0.62, fraction: 0.74, sway: 0.05 },
};

/** opts: { count = 5 (2..7), seed } */
export function createTidepool(opts = {}) {
  const count = Math.max(2, Math.min(7, opts.count ?? 4));
  const R = rng(opts.seed ?? 7);
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(5.0, 3.6, 0.3));

  const P = { x: -0.4, z: -0.25, rx: 1.55, rz: 1.05 };
  // water
  const water = k.mesh(new THREE.CircleGeometry(1, 32).rotateX(-Math.PI / 2), k.toon(C.teal500, { transparent: true, opacity: 0.82 }), { outline: false, cast: false, receive: true });
  water.scale.set(P.rx, 1, P.rz);
  water.position.set(P.x, 0.02, P.z);
  g.add(water);
  // rock rim
  const mRock = k.toon(C.rock);
  const rockGeo = new THREE.DodecahedronGeometry(0.13, 0);
  for (let i = 0; i < 20; i++) {
    const a = (i / 20) * TAU + R() * 0.12;
    const rock = k.mesh(rockGeo, mRock, { receive: true });
    rock.position.set(P.x + Math.cos(a) * (P.rx + 0.1), 0.04, P.z + Math.sin(a) * (P.rz + 0.1));
    rock.scale.set(1.15 + R() * 0.6, 0.5 + R() * 0.4, 0.9 + R() * 0.4);
    rock.rotation.y = R() * TAU;
    g.add(rock);
  }
  // ripple ring
  const ripple = new THREE.Mesh(k.geo(new THREE.RingGeometry(0.92, 1, 40).rotateX(-Math.PI / 2)), k.basic(C.paper, { transparent: true, depthWrite: false }));
  ripple.position.set(P.x, 0.03, P.z);
  g.add(ripple);

  // crabs
  const crabs = [];
  for (let i = 0; i < count; i++) {
    const c = createCrab({ scale: 0.21, shell: 'grey', pose: 'idle', phase: i * 0.37 });
    const a = (i / count) * TAU;
    crabs.push({
      c, cx: P.x + Math.cos(a) * 0.72, cz: P.z + Math.sin(a) * 0.45,
      ax: 0.16 + R() * 0.1, az: 0.08 + R() * 0.06, k1: 1 + (i % 3), k2: 2 + (i % 2), ph: R() * TAU, yaw: R() * TAU,
    });
    g.add(c.object3d);
  }

  // striped caution barrier on the sand, between viewer and pool
  const barrier = new THREE.Group();
  const mInk = k.toon(C.ink);
  const mStripe = k.toon(C.paper);
  const legGeo = new THREE.BoxGeometry(0.06, 0.6, 0.06);
  for (const sx of [-0.55, 0.55]) {
    for (const sz of [-0.13, 0.13]) {
      const leg = k.mesh(legGeo, mInk);
      leg.position.set(sx, 0.28, sz);
      leg.rotation.x = sz > 0 ? -0.22 : 0.22;
      barrier.add(leg);
    }
  }
  const segs = 7, BW = 1.4;
  for (let i = 0; i < segs; i++) {
    const seg = k.mesh(new THREE.BoxGeometry(BW / segs, 0.2, 0.07), i % 2 ? mInk : mStripe);
    seg.position.set(-BW / 2 + (i + 0.5) * (BW / segs), 0.52, 0);
    barrier.add(seg);
  }
  barrier.position.set(1.35, 0, 1.25);
  barrier.rotation.y = -0.15;
  g.add(barrier);

  function update(t, dt) {
    const L = tidepoolMeta.loop;
    for (const s of crabs) {
      const w1 = (TAU * s.k1) / L, w2 = (TAU * s.k2) / L;
      const x = s.cx + s.ax * Math.sin(w1 * t + s.ph);
      const z = s.cz + s.az * Math.sin(w2 * t + s.ph * 1.3);
      const vx = s.ax * w1 * Math.cos(w1 * t + s.ph);
      const vz = s.az * w2 * Math.cos(w2 * t + s.ph * 1.3);
      s.c.object3d.position.set(x, 0.02, z);
      // crabs walk sideways: face perpendicular to travel, gently
      s.c.object3d.rotation.y = s.yaw + 0.25 * Math.sin(w1 * t);
      s.c.setWalk(Math.min(1, Math.hypot(vx, vz) * 2.2));
      s.c.update(t, dt);
    }
    const u = (t % 3) / 3;
    ripple.scale.set(P.rx * (0.15 + u * 0.75), 1, P.rz * (0.15 + u * 0.75));
    ripple.material.opacity = 0.45 * Math.sin(Math.PI * u);
  }

  return {
    object3d: g, meta: tidepoolMeta, crabs: crabs.map((s) => s.c), update,
    dispose() { crabs.forEach((s) => s.c.dispose()); k.dispose(); },
  };
}
