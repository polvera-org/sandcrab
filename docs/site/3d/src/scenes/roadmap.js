// Roadmap = stepping stones into the water. From the V1 shore the crab walks over
// four stones: a KVM host, Firecracker numbers, Cloud Hypervisor + libkrun jars,
// and a lighthouse (the control plane). Each stone lights as the crab reaches it.
// One idea: "next steps, in order; each one unlocks the next."
// Phases (16 s): start 1.5 → 4 × (walk 1.6 → pause 1.4) → end 1.5 → fade/reset 1.
// Motion budget: the crab, one stone glow at a time, slow sway.
import { THREE, createKit, PALETTE as C, ease, win, clamp01, lerp } from '../kit.js';
import { createCrab } from '../crab.js';
import { createJarBody } from '../jar.js';
import { anchor, label, serverTower, loopU } from './common.js';

export const roadmapMeta = {
  title: 'Roadmap',
  fallback: 'reef',
  alt: 'A crab walks across four stepping stones: KVM host, Firecracker numbers, Cloud Hypervisor and libkrun, control plane',
  loop: 16,
  still: 14.2,
  phases: [{ name: 'start', d: 1.5 }, { name: 'walk', d: 12 }, { name: 'end', d: 1.5 }, { name: 'reset', d: 1 }],
  view: { target: [0.2, 0.35, 0], radius: 3.4, yaw: 0.3, pitch: 0.55, fraction: 0.92, sway: 0.03 },
};

const STONES = [[-1.5, 0.75], [0.0, 0.15], [1.5, -0.35], [3.0, -0.85]];
const FRONT = 0.22; // crab offset toward the viewer on a stone
const START = [-3.0, 1.15];

export function createRoadmap() {
  const k = createKit();
  const g = new THREE.Group();
  // shore on the left, sea everywhere else
  const sea = k.mesh(new THREE.BoxGeometry(6.4, 0.22, 3.6).translate(0.9, -0.19, 0), k.toon(C.teal500), { receive: true });
  g.add(sea);
  const shore = k.slab(1.8, 3.6, 0.32);
  shore.position.set(-3.1, 0.02, 0);
  g.add(shore);

  const mRock = k.toon(C.rock);
  const stones = STONES.map(([x, z]) => {
    const s = k.mesh(new THREE.CylinderGeometry(0.62, 0.7, 0.3, 9).translate(0, -0.1, 0), mRock, { receive: true });
    s.position.set(x, 0.05, z);
    g.add(s);
    const ringMat = k.basic(C.teal500, { transparent: true, opacity: 0, depthWrite: false });
    const ring = new THREE.Mesh(k.geo(new THREE.RingGeometry(0.66, 0.76, 32).rotateX(-Math.PI / 2)), ringMat);
    ring.position.set(x, 0.07, z);
    g.add(ring);
    return { s, ringMat, x, z };
  });

  // props on each stone
  const tower = serverTower(k, 0.42, 0.62, 0.42);
  tower.group.position.set(STONES[0][0] + 0.1, 0.05, STONES[0][1] - 0.28);
  g.add(tower.group);
  const bars = new THREE.Group();
  [0.5, 0.25, 0.38].forEach((h, i) => {
    const b = k.mesh(new THREE.BoxGeometry(0.16, h, 0.16).translate(0, h / 2, 0), k.toon(i === 1 ? C.teal700 : C.teal500));
    b.position.set(-0.22 + i * 0.22, 0, 0);
    bars.add(b);
  });
  bars.position.set(STONES[1][0] + 0.05, 0.05, STONES[1][1] - 0.3);
  g.add(bars);
  const jars = [-0.22, 0.22].map((dx, i) => {
    const j = createJarBody({ scale: 0.24, sealed: true, crab: { pose: 'idle', phase: i * 0.5 } });
    j.object3d.position.set(STONES[2][0] + dx, 0.05, STONES[2][1] - 0.3);
    g.add(j.object3d);
    return j;
  });
  const light = new THREE.Group();
  const lh = k.mesh(new THREE.CylinderGeometry(0.13, 0.2, 0.8, 12).translate(0, 0.4, 0), k.toon(C.paper));
  const band = k.mesh(new THREE.CylinderGeometry(0.165, 0.18, 0.12, 12).translate(0, 0.3, 0), k.toon(C.teal700), { outline: false });
  const lampMat = k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.2 });
  const lamp = k.mesh(new THREE.BoxGeometry(0.2, 0.16, 0.2).translate(0, 0.88, 0), lampMat);
  const roof = k.mesh(new THREE.ConeGeometry(0.17, 0.18, 12).translate(0, 1.05, 0), k.toon(C.ink));
  light.add(lh, band, lamp, roof);
  light.position.set(STONES[3][0] + 0.15, 0.05, STONES[3][1] - 0.32);
  g.add(light);

  const crab = createCrab({ scale: 0.3, pose: 'idle', detail: 'low' });
  g.add(crab.object3d);

  const names = ['1 · KVM host', '2 · Firecracker numbers', '3 · Cloud Hypervisor + libkrun', '4 · control plane'];
  const labels = [
    label('V1 today', anchor(g, START[0], 0.75, START[1]), { cls: 'quiet' }),
    ...STONES.map(([x, z], i) => label(names[i], i % 2 ? anchor(g, x, 0.05, z + 0.72) : anchor(g, x, i === 0 ? 0.95 : 1.45, z - 0.3), { a: 0.45, below: i % 2 === 1 })),
  ];

  function update(t, dt) {
    const u = loopU(t, roadmapMeta.loop);
    // walk schedule: stone i reached at 1.5 + i*3 + 1.6
    let x = START[0], z = START[1], walk = 0, reached = -1;
    const w = u - 1.5;
    if (w >= 0) {
      const i = Math.min(3, Math.floor(w / 3));
      const p = w - i * 3;
      const from = i === 0 ? START : STONES[i - 1], to = STONES[i];
      if (w >= 12) { x = STONES[3][0]; z = STONES[3][1]; reached = 3; }
      else if (p < 1.6) { const e = ease(p / 1.6); x = lerp(from[0], to[0], e); z = lerp(from[1], to[1], e); walk = Math.sin(Math.PI * p / 1.6) * 1.1; reached = i - 1; }
      else { x = to[0]; z = to[1]; reached = i; }
    }
    const fade = u >= 15 ? 1 - win(u, 15, 15.5) + win(u, 15.5, 16) : 1;
    if (u >= 15.5) { x = START[0]; z = START[1]; reached = -1; }
    crab.object3d.position.set(x, x < -2.2 ? 0.02 : 0.05, z + FRONT * clamp01((x + 2.6) / 0.8));
    crab.object3d.rotation.y = -0.35;
    crab.setWalk(walk);
    crab.setOpacity(fade);
    crab.setPose(reached === 3 && u < 15 ? 'wave' : 'idle');
    crab.update(t, dt);
    jars.forEach((j) => j.update(t, dt));
    stones.forEach((s, i) => {
      const on = i <= reached;
      s.ringMat.opacity = on ? 0.85 : 0;
      labels[i + 1].a = on ? 1 : 0.45;
    });
    lampMat.emissiveIntensity = reached === 3 ? 0.6 + 0.6 * (0.5 + 0.5 * Math.sin(u * Math.PI)) : 0.2;
  }

  return {
    object3d: g, meta: roadmapMeta, labels, update,
    dispose() { crab.dispose(); jars.forEach((j) => j.dispose()); k.dispose(); },
  };
}
