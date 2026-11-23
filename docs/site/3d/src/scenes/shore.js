// Shore = title. The crab on the shoreline beside white sandcastle blocks (the
// README hero), the tide breathing in and out behind it.
// One idea: "an architect of the impermanent, carving safe harbours."
// Phases (12 s): one tide cycle every 6 s (two per loop); crab waves throughout.
// Motion budget: crab wave, foam line, slow camera sway.
import { THREE, createKit, PALETTE as C, TAU } from '../kit.js';
import { createCrab } from '../crab.js';

export const shoreMeta = {
  title: 'Shore',
  fallback: 'reef',
  alt: 'The Sandcrab crab waving on the shoreline beside white sandcastle blocks',
  loop: 12,
  still: 1.4,
  phases: [{ name: 'tide', d: 12 }],
  view: { target: [0.1, 0.55, 0.2], radius: 2.4, yaw: 0.5, pitch: 0.48, fraction: 0.82, sway: 0.06 },
};

export function createShore() {
  const k = createKit();
  const g = new THREE.Group();
  // sand (front) and sea (back)
  const sand = k.slab(6, 2.6, 0.3);
  sand.position.z = 0.7;
  g.add(sand);
  const sea = k.mesh(new THREE.BoxGeometry(6, 0.24, 1.5).translate(0, -0.18, -1.35), k.toon(C.teal500), { receive: true });
  g.add(sea);
  const deep = k.mesh(new THREE.BoxGeometry(6, 0.02, 0.5).translate(0, -0.055, -1.85), k.toon(C.teal700), { outline: false, cast: false });
  g.add(deep);
  // foam: two paper strips that ride the tide over the wet sand edge
  const mFoam = k.basic(C.paper, { transparent: true, opacity: 0.9, depthWrite: false });
  const foam = [0, 1].map((i) => {
    const f = new THREE.Mesh(k.geo(new THREE.BoxGeometry(6, 0.02, 0.07 - i * 0.025)), mFoam);
    g.add(f);
    return f;
  });
  const wet = new THREE.Mesh(k.geo(new THREE.PlaneGeometry(6, 0.5).rotateX(-Math.PI / 2)), k.basic(C.sandDeep, {
    transparent: true, opacity: 0.35, depthWrite: false, polygonOffset: true, polygonOffsetFactor: -1, polygonOffsetUnits: -1,
  }));
  wet.position.set(0, 0.006, -0.38);
  g.add(wet);

  // sandcastle: white blocks with crenellations
  const castle = new THREE.Group();
  const mWhite = k.toon(C.paper);
  const blk = (w, h, d, x, y, z) => { const b = k.mesh(new THREE.BoxGeometry(w, h, d).translate(0, h / 2, 0), mWhite, { receive: true }); b.position.set(x, y, z); castle.add(b); return b; };
  blk(0.9, 0.55, 0.8, 0, 0, 0);
  blk(0.5, 0.45, 0.45, 0.02, 0.55, 0.0);
  for (const [x, z] of [[-0.36, -0.31], [0.36, -0.31], [-0.36, 0.31], [0.36, 0.31]]) blk(0.16, 0.14, 0.16, x, 0.55, z);
  blk(0.55, 0.3, 0.5, 0.82, 0, 0.35);
  blk(0.4, 0.22, 0.4, -0.7, 0, 0.55);
  const door = k.mesh(new THREE.BoxGeometry(0.2, 0.28, 0.02).translate(0, 0.14, 0), k.toon(C.teal700), { outline: false });
  door.position.set(0, 0, 0.41);
  castle.add(door);
  castle.position.set(-1.55, 0, 0.15);
  castle.rotation.y = 0.25;
  g.add(castle);

  // the crab
  const crab = createCrab({ scale: 0.82, pose: 'wave' });
  crab.object3d.position.set(0.85, 0, 0.75);
  crab.object3d.rotation.y = -0.15;
  g.add(crab.object3d);

  // three shells/pebbles for scale
  const mRock = k.toon(C.rock);
  [[2.3, 1.3, 0.12], [2.55, 0.95, 0.08], [-2.4, 1.5, 0.1]].forEach(([x, z, r]) => {
    const p = k.mesh(new THREE.DodecahedronGeometry(r, 0), mRock);
    p.position.set(x, r * 0.4, z);
    p.scale.y = 0.55;
    g.add(p);
  });

  function update(t, dt) {
    crab.update(t, dt);
    const tide = Math.sin((TAU * t) / 6);
    foam[0].position.set(0, 0.01, -0.5 + 0.16 * tide);
    foam[1].position.set(0, 0.01, -0.62 + 0.12 * Math.sin((TAU * t) / 6 - 0.8));
    mFoam.opacity = 0.65 + 0.3 * (0.5 + 0.5 * tide);
    wet.position.z = -0.42 + 0.08 * tide;
  }

  return { object3d: g, meta: shoreMeta, labels: [], update, dispose() { crab.dispose(); k.dispose(); } };
}
