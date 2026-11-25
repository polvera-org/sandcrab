// Isolation = what isolates what, stated honestly. Left: the kit tide pool
// (Docker, shared host kernel, hardened, for single-tenant trusted hosts). Right: the kit sealed jar
// (Firecracker, own kernel behind KVM). In front of each, one flag per declared
// capability: raised = true, lowered = false — read from the backends' source.
// One idea: "a jar is not a pool, and each backend says which it is."
// Phases (12 s): flags rise once (0.4–3.2 s, staggered), then both kit loops play.
// Motion budget: pool crabs (kit), jar lid (kit), flags only on entry.
import { THREE, createKit, PALETTE as C, ease, clamp01 } from '../kit.js';
import { createTidepool } from '../tidepool.js';
import { createJar } from '../jar.js';
import { anchor, label } from './common.js';

export const isolationMeta = {
  title: 'Isolation',
  fallback: 'isolation',
  alt: 'A shared tide pool of grey crabs (Docker) beside a crab sealed in its own jar (Firecracker), each with capability flags',
  loop: 12,
  still: 7,
  phases: [{ name: 'flags', d: 3.2 }, { name: 'hold', d: 8.8 }],
  view: { target: [0, 0.7, 0.25], radius: 3.25, yaw: 0.3, pitch: 0.5, fraction: 0.92, sway: 0.03 },
};

const CAPS = [
  ['hardware_isolated', 'hw isolated'],
  ['multi_tenant_safe', 'multi-tenant'],
  ['supports_host_dir_mounts', 'host-dir mounts'],
  ['supports_snapshots', 'snapshots'],
];
const DEFAULT_CAPS = {
  'docker-dev': { hardware_isolated: false, multi_tenant_safe: false, supports_snapshots: false, supports_host_dir_mounts: true },
  firecracker: { hardware_isolated: true, multi_tenant_safe: false, supports_snapshots: false, supports_host_dir_mounts: false },
};

/** opts.data.caps = BackendCapabilities literals per backend. */
export function createIsolation(opts = {}) {
  const caps = opts.data?.caps || DEFAULT_CAPS;
  const k = createKit();
  const g = new THREE.Group();

  const pool = createTidepool({ count: 4 });
  pool.object3d.scale.setScalar(0.62);
  pool.object3d.position.set(-2.15, 0, -0.25);
  g.add(pool.object3d);
  const jar = createJar();
  jar.object3d.scale.setScalar(0.72);
  jar.object3d.position.set(2.2, 0, -0.3);
  g.add(jar.object3d);

  // capability flag rows (on a thin sand strip in front of each backend)
  const flags = [];
  const mPole = k.toon(C.ink);
  const mTrue = k.toon(C.teal500), mFalse = k.toon(C.paper);
  function row(name, cx, cz) {
    const strip = k.slab(2.3, 0.55, 0.12);
    strip.position.set(cx, 0, cz);
    g.add(strip);
    CAPS.forEach(([key], i) => {
      const on = !!caps[name]?.[key];
      const x = cx - 0.84 + i * 0.56;
      const pole = k.mesh(new THREE.CylinderGeometry(0.018, 0.018, 0.62, 6).translate(0, 0.31, 0), mPole, { outline: false });
      pole.position.set(x, 0, cz);
      g.add(pole);
      const flag = k.mesh(new THREE.BoxGeometry(0.24, 0.15, 0.02).translate(0.12, -0.075, 0), on ? mTrue : mFalse);
      flag.position.set(x + 0.02, 0.16, cz);
      g.add(flag);
      flags.push({ flag, on, delay: 0.4 + i * 0.35 + (name === 'firecracker' ? 0.15 : 0) });
    });
  }
  row('docker-dev', -2.15, 1.55);
  row('firecracker', 2.2, 1.55);

  const yes = (n, key) => (caps[n]?.[key] ? '✓' : '✗');
  const txt = (n) => CAPS.map(([key, short]) => `${yes(n, key)} ${short}`).join('\n');
  const labels = [
    label('Docker · shared kernel', anchor(g, -2.15, 1.25, -0.25)),
    label('Firecracker · own kernel', anchor(g, 2.2, 2.05, -0.3)),
    label(txt('docker-dev'), anchor(g, -2.15, 0, 1.85), { cls: 'mono', below: true }),
    label(txt('firecracker'), anchor(g, 2.2, 0, 1.85), { cls: 'mono', below: true }),
  ];

  function update(t, dt) {
    pool.update(t, dt);
    jar.update(t, dt);
    for (const f of flags) {
      const p = ease(clamp01((t - f.delay) / 0.9));
      // raised flags climb to the top of the pole; lowered ones stay at the bottom
      f.flag.position.y = 0.16 + (f.on ? 0.46 * p : 0);
    }
  }

  return {
    object3d: g, meta: isolationMeta, labels, update,
    dispose() { pool.dispose(); jar.dispose(); k.dispose(); },
  };
}
