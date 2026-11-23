// Frames = one protocol, many callers, two transports. SDK, CLI and crabagent send
// length-prefixed JSON-RPC frames (4 header bytes + JSON body) through
// sandcrab-core to crabd: over a Unix socket into the Docker pool, over vsock into
// the Firecracker jar. The result (teal cube) comes back the same way.
// One idea: "same frames, whatever the caller, whatever the backend."
// Phases (12 s): four exchanges × (to-core 0.8 → to-crabd 0.8 → work 0.4 → back 1.0).
// Motion budget: one packet at a time, the receiving crab's work pose, slow sway.
import { THREE, createKit, PALETTE as C, ease, clamp01, timeline } from '../kit.js';
import { createCrab } from '../crab.js';
import { createJarBody } from '../jar.js';
import { anchor, label, cable, framePacket, miniPool, textTexture, loopU } from './common.js';

const ONE = [{ name: 'to-core', d: 0.8 }, { name: 'to-crabd', d: 0.8 }, { name: 'work', d: 0.4 }, { name: 'back', d: 1.0 }];
const EXCHANGES = [[0, 'D'], [2, 'F'], [1, 'D'], [0, 'F']];
const PHASES = EXCHANGES.flatMap(() => ONE);

export const framesMeta = {
  title: 'Frames',
  fallback: 'one-wire',
  alt: 'SDK, CLI and crabagent send the same length-prefixed frames through one core to crabd in a Docker pool and a Firecracker jar',
  loop: 12,
  still: 0.6,
  phases: PHASES,
  view: { target: [0, 0.45, 0], radius: 2.85, yaw: 0.36, pitch: 0.56, fraction: 0.86, sway: 0.04 },
};

const CLIENTS = [['SDK', -1.05], ['CLI', 0], ['crab', 1.05]];

export function createFrames() {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(6.6, 3.6, 0.3));

  // callers: three small terminals with their names on the screen
  const clientPos = [];
  CLIENTS.forEach(([name, z], i) => {
    const term = new THREE.Group();
    const face = k.toon('#FFFFFF', { map: textTexture(k, i === 2 ? ['crab', 'agent'] : [name], { font: '700 62px "JetBrains Mono", ui-monospace, monospace' }) });
    const shell = k.toon(C.teal700);
    term.add(k.mesh(new THREE.BoxGeometry(0.62, 0.5, 0.5).translate(0, 0.25, 0), [shell, shell, shell, shell, face, shell]));
    term.position.set(-2.65, 0, z);
    g.add(term);
    clientPos.push([-2.32, 0.18, z]);
  });

  // sandcrab-core hub
  const core = k.mesh(new THREE.BoxGeometry(0.5, 0.42, 0.5).translate(0, 0.21, 0), k.toon(C.ink));
  core.position.set(-0.75, 0, 0);
  g.add(core);
  const coreLed = k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.3 });
  const led = k.mesh(new THREE.BoxGeometry(0.1, 0.1, 0.02), coreLed, { outline: false });
  led.position.set(-0.75, 0.3, 0.26);
  g.add(led);

  // backends: Docker tide pool (UDS) and Firecracker jar (vsock)
  const pool = miniPool(k, 0.62);
  pool.position.set(2.05, 0, -0.95);
  g.add(pool);
  const poolCrab = createCrab({ scale: 0.24, shell: 'grey', pose: 'idle', detail: 'low' });
  poolCrab.object3d.position.set(2.05, 0.02, -0.95);
  g.add(poolCrab.object3d);
  const jar = createJarBody({ scale: 0.42, sealed: true, crab: { pose: 'idle' } });
  jar.object3d.position.set(2.05, 0, 0.95);
  g.add(jar.object3d);

  // cables: callers → core, core → each backend
  const inCurves = clientPos.map((p) => cable(k, [p, [-1.7, 0.06, p[2] * 0.7], [-1.2, 0.06, p[2] * 0.25], [-1.0, 0.16, 0]]));
  inCurves.forEach((c) => g.add(c.mesh));
  const outD = cable(k, [[-0.5, 0.16, 0], [0.2, 0.06, -0.2], [1.0, 0.06, -0.85], [1.5, 0.1, -0.95]]);
  const outF = cable(k, [[-0.5, 0.16, 0], [0.2, 0.06, 0.2], [1.0, 0.06, 0.85], [1.62, 0.18, 0.95]]);
  g.add(outD.mesh, outF.mesh);

  const pkt = framePacket(k);
  pkt.scale.setScalar(1.5);
  g.add(pkt);
  const res = k.mesh(new THREE.BoxGeometry(0.17, 0.17, 0.17), k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.45 }));
  g.add(res);

  const labels = [
    label('SDK · CLI · crabagent', anchor(g, -2.65, 0.85, 0)),
    label('u32 length + JSON-RPC 2.0', anchor(g, -0.75, 0.75, 0), { a: 0.9 }),
    label('Unix socket → crabd · Docker', anchor(g, 2.05, 0.6, -0.95)),
    label('vsock → crabd · Firecracker', anchor(g, 2.05, 0, 1.5), { below: true }),
  ];

  const p = new THREE.Vector3(), q = new THREE.Vector3();
  function along(obj, curve, s, lift) {
    curve.getPointAt(clamp01(s), p);
    curve.getPointAt(clamp01(s + 0.01), q);
    obj.position.set(p.x, p.y + lift, p.z);
    obj.rotation.y = Math.atan2(-(q.z - p.z), q.x - p.x);
  }

  function update(t, dt) {
    const L = framesMeta.loop;
    const u = loopU(t, L);
    const tl = timeline(PHASES, u);
    const ex = Math.min(EXCHANGES.length - 1, Math.floor(u / 3));
    const [ci, tgt] = EXCHANGES[ex];
    const inC = inCurves[ci].curve, outC = tgt === 'D' ? outD.curve : outF.curve;
    pkt.visible = tl.phase === 'to-core' || tl.phase === 'to-crabd';
    res.visible = tl.phase === 'back';
    if (tl.phase === 'to-core') along(pkt, inC, ease(tl.p), 0.16);
    if (tl.phase === 'to-crabd') along(pkt, outC, ease(tl.p), 0.16);
    if (res.visible) {
      const e = ease(tl.p);
      if (e < 0.5) { outC.getPointAt(1 - e * 2, p); } else { inC.getPointAt(1 - (e - 0.5) * 2, p); }
      res.position.set(p.x, p.y + 0.15, p.z);
    }
    const working = tl.phase === 'work';
    jar.crab.setPose(working && tgt === 'F' ? 'work' : 'idle');
    poolCrab.setPose(working && tgt === 'D' ? 'work' : 'idle');
    jar.update(t, dt);
    poolCrab.update(t, dt);
    coreLed.emissiveIntensity = 0.3 + (tl.phase === 'to-crabd' ? 1.2 * Math.exp(-tl.p * 4) : 0);
    // the active transport's label is full strength, the other one dims
    const active = tl.phase === 'to-core' ? null : tgt;
    labels[2].a = active === 'F' ? 0.45 : 1;
    labels[3].a = active === 'D' ? 0.45 : 1;
  }

  return {
    object3d: g, meta: framesMeta, labels, update,
    dispose() { poolCrab.dispose(); jar.dispose(); k.dispose(); },
  };
}
