// Molting = ephemeral sandbox teardown. The crab steps out of its shell carrying
// its output (a teal cube); the empty husk cracks and blows away as sand; the crab
// and its output persist. One idea: "the sandbox is discarded, the output survives."
// Phases (12 s): idle 2.5 → step out 2 → crack 0.8 → dissolve 1.7 → hold 2.5 → return 2.5.
// Motion budget: crab walk, husk crack/dissolve (one at a time), sand drift, slow sway.
import { THREE, createKit, PALETTE as C, win, clamp01, lerp, loopLength, timeline, rng, ease } from './kit.js';
import { createCrab } from './crab.js';

const PHASES = [
  { name: 'idle', d: 2.5 }, { name: 'step out', d: 2 }, { name: 'crack', d: 0.8 },
  { name: 'dissolve', d: 1.7 }, { name: 'hold', d: 2.5 }, { name: 'return', d: 2.5 },
];

export const moltMeta = {
  title: 'Molting',
  fallback: 'molting',
  alt: 'A crab steps out of its old shell, which crumbles to sand',
  loop: loopLength(PHASES),
  still: 5.0,
  phases: PHASES,
  view: { target: [0.6, 0.35, 0.1], radius: 2.2, yaw: 0.4, pitch: 0.5, fraction: 0.76, sway: 0.05 },
};

const HUSK_X = -0.55, OUT_X = 1.7;

export function createMolt(opts = {}) {
  const k = createKit();
  const g = new THREE.Group();
  const slab = k.slab(4.6, 3.0, 0.3);
  slab.position.x = 0.6;
  g.add(slab);

  const crab = createCrab({ scale: 0.6, pose: 'idle', ...(opts.crab || {}) });
  g.add(crab.object3d);
  const husk = createCrab({ scale: 0.6, shell: 'husk', split: true, phase: 0.4 });
  husk.object3d.position.x = HUSK_X;
  g.add(husk.object3d);

  // the turn's output: a glowing teal cube that travels with the crab
  const cube = k.mesh(new THREE.BoxGeometry(0.26, 0.26, 0.26), k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.45 }));
  g.add(cube);

  // sand grains the husk dissolves into (one instanced draw call)
  const N = 42;
  const R = rng(11);
  const grains = new THREE.InstancedMesh(k.geo(new THREE.BoxGeometry(0.06, 0.06, 0.06)), k.toon(C.sandDeep), N);
  grains.castShadow = false;
  const seeds = Array.from({ length: N }, () => ({
    x: (R() - 0.5) * 1.1, y: 0.25 + R() * 0.5, z: (R() - 0.5) * 0.9,
    dx: 0.25 + R() * 0.45, dy: 0.1 + R() * 0.35, d: R() * 0.35, rot: R() * 6,
  }));
  g.add(grains);
  const m4 = new THREE.Matrix4(), q = new THREE.Quaternion(), e = new THREE.Euler(), v = new THREE.Vector3(), s3 = new THREE.Vector3();

  function update(t, dt) {
    const L = moltMeta.loop;
    const u = ((t % L) + L) % L;
    const tl = timeline(PHASES, u);
    const at = (name) => PHASES.slice(0, PHASES.findIndex((p) => p.name === name)).reduce((a, p) => a + p.d, 0);

    // crab position: at husk spot → walks out → holds → walks back
    let x = HUSK_X, walk = 0;
    if (tl.phase === 'step out') { x = lerp(HUSK_X, OUT_X, win(u, tl.from, tl.to)); walk = Math.sin(Math.PI * tl.p); }
    else if (['crack', 'dissolve', 'hold'].includes(tl.phase)) x = OUT_X;
    else if (tl.phase === 'return') { x = lerp(OUT_X, HUSK_X, win(u, tl.from, tl.to)); walk = Math.sin(Math.PI * tl.p); }
    crab.object3d.position.x = x;
    crab.setWalk(walk);
    crab.setPose(tl.phase === 'hold' ? 'wave' : 'idle');
    crab.update(t, dt);
    cube.position.set(x + 0.05, 0.13, 1.05);
    cube.rotation.y = 0.3;

    // husk: appears as the crab leaves, cracks, dissolves; absent while the crab is inside
    const tStep = at('step out'), tCrack = at('crack'), tDis = at('dissolve');
    let ha = 0, crack = 0;
    if (u >= tStep && u < tCrack) ha = win(u, tStep + 0.5, tStep + 1.1);
    else if (u >= tCrack && u < tDis) { ha = 1; crack = win(u, tCrack, tDis) * 0.55; }
    else if (u >= tDis && u < tDis + 1.7) { crack = 0.55; ha = 1 - win(u, tDis, tDis + 0.9); }
    husk.object3d.visible = ha > 0.01;
    husk.setOpacity(ha);
    husk.crack(crack);
    husk.object3d.position.y = -0.12 * clamp01((u - tDis) / 1.2) * (u >= tDis && u < tDis + 1.7 ? 1 : 0);
    husk.update(1.0, 0); // husk is lifeless: frozen pose

    // grains: burst from the husk during dissolve, drift left and settle
    const gp = (u - tDis) / 2.4;
    grains.visible = gp > 0 && gp < 1;
    if (grains.visible) {
      seeds.forEach((s, i) => {
        const p = clamp01((gp - s.d) / (1 - s.d));
        const ep = ease(p);
        v.set(HUSK_X + s.x - s.dx * ep, Math.max(0.03, s.y + s.dy * Math.sin(Math.PI * p) - s.y * ep), s.z - 0.2 * ep);
        const sc = p <= 0 ? 0 : (1 - p) * 1.1;
        s3.setScalar(Math.max(0.0001, sc));
        e.set(s.rot * p, s.rot, 0);
        q.setFromEuler(e);
        m4.compose(v, q, s3);
        grains.setMatrixAt(i, m4);
      });
      grains.instanceMatrix.needsUpdate = true;
    }
  }

  return {
    object3d: g, meta: moltMeta, crab, husk, update,
    dispose() { crab.dispose(); husk.dispose(); k.dispose(); },
  };
}
