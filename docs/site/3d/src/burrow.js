// Burrow = persistent workspace. A cutaway sand block with a chamber of crates
// (files). A crab (sandbox) spawns on the surface, writes one glowing crate down
// the shaft, then molts away; the crates stay put and keep their glow.
// One idea: "sandboxes come and go; the workspace stays."
// Phases (10 s): spawn 1 → walk 1.8 → write 1.8 → teardown 1.8 → persist 3.6.
// Motion budget: crab (spawn/walk/fade), one write pulse, slow sway. Crates never move.
import { THREE, createKit, PALETTE as C, win, clamp01, lerp, loopLength, timeline, ease } from './kit.js';
import { createCrab } from './crab.js';

const PHASES = [
  { name: 'spawn', d: 1 }, { name: 'walk', d: 1.8 }, { name: 'write', d: 1.8 },
  { name: 'teardown', d: 1.8 }, { name: 'persist', d: 3.6 },
];

export const burrowMeta = {
  title: 'Burrow',
  fallback: 'burrow',
  alt: 'Cutaway of a sand burrow full of crates that persist while crabs come and go',
  loop: loopLength(PHASES),
  still: 3.9,
  phases: PHASES,
  groundY: -1.8,
  view: { target: [0, -0.6, 0.2], radius: 2.3, yaw: 0.32, pitch: 0.42, fraction: 0.74, sway: 0.05 },
};

export function createBurrow(opts = {}) {
  const k = createKit();
  const g = new THREE.Group();
  const W = 4.2, D = 3.0, H = 1.8;
  const cav = { x0: -1.45, x1: 1.45, y0: -1.55, y1: -0.3, z0: -0.25 };
  const mSand = k.toon(C.sand200);
  const mDeep = k.toon(C.sandDeep);
  // only the roof receives shadows, so the chamber reads as lit, like the cutaway illustration
  const block = (x0, x1, y0, y1, z0, z1, m = mSand, receive = false) => {
    const geo = new THREE.BoxGeometry(x1 - x0, y1 - y0, z1 - z0).translate((x0 + x1) / 2, (y0 + y1) / 2, (z0 + z1) / 2);
    const b = k.mesh(geo, m, { receive });
    g.add(b);
    return b;
  };
  // five blocks around a chamber open to the front (+Z) face
  block(-W / 2, W / 2, cav.y1, 0, -D / 2, D / 2, mSand, true); // roof
  block(-W / 2, W / 2, -H, cav.y0, -D / 2, D / 2);           // floor
  block(-W / 2, cav.x0, cav.y0, cav.y1, -D / 2, D / 2);      // left wall
  block(cav.x1, W / 2, cav.y0, cav.y1, -D / 2, D / 2);       // right wall
  block(cav.x0, cav.x1, cav.y0, cav.y1, -D / 2, cav.z0, mDeep); // back wall

  // shaft opening on the surface
  const hole = new THREE.Mesh(k.geo(new THREE.CircleGeometry(0.2, 24).rotateX(-Math.PI / 2)), k.basic(C.ink, { transparent: true, opacity: 0.55, depthWrite: false }));
  hole.scale.z = 0.8;
  hole.position.set(0.05, 0.004, 0.75);
  g.add(hole);

  // crates (files) in the chamber. Never move. Each keeps a faint persisted glow.
  const S = 0.44;
  const crateGeo = new THREE.BoxGeometry(S, S, S);
  const bandGeo = new THREE.BoxGeometry(S + 0.02, 0.07, S + 0.02);
  const crates = [];
  const addCrate = (x, y, z, glow) => {
    const m = k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: glow });
    const c = k.mesh(crateGeo, m);
    c.position.set(x, y, z);
    const band = k.mesh(bandGeo, k.toon(C.teal700), { outline: false });
    c.add(band);
    g.add(c);
    crates.push({ c, m, base: glow });
    return crates[crates.length - 1];
  };
  const fy = cav.y0 + S / 2;
  addCrate(-0.95, fy, 0.55, 0.12);
  addCrate(-0.45, fy, 0.85, 0.12);
  addCrate(-0.85, fy + S, 0.6, 0.12);
  const target = addCrate(0.05, fy, 0.75, 0.22);
  // a folder of papers beside the crates
  const folder = k.mesh(new THREE.BoxGeometry(0.5, 0.3, 0.36), k.toon(C.teal700), { receive: true });
  folder.position.set(0.75, cav.y0 + 0.15, 0.9);
  g.add(folder);
  for (let i = 0; i < 3; i++) {
    const sheet = k.mesh(new THREE.BoxGeometry(0.03, 0.26, 0.3), k.toon(C.paper), { outline: i === 0 });
    sheet.position.set(0.61 + i * 0.1, cav.y0 + 0.36, 0.9);
    g.add(sheet);
  }

  // write pulse: a small glowing cube carried from claw → shaft → target crate
  const pulse = k.mesh(new THREE.BoxGeometry(0.16, 0.16, 0.16), k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.9 }), { cast: false });
  g.add(pulse);

  const crab = createCrab({ scale: 0.42, pose: 'idle' });
  g.add(crab.object3d);
  const X_SPAWN = 1.4, X_WORK = 0.62, Z = 0.7;

  function update(t, dt) {
    const L = burrowMeta.loop;
    const u = ((t % L) + L) % L;
    const tl = timeline(PHASES, u);
    let x = X_WORK, sc = 1, fade = 1, sink = 0, walk = 0, tint = 0;
    if (tl.phase === 'spawn') { x = X_SPAWN; sc = ease(tl.p); }
    else if (tl.phase === 'walk') { x = lerp(X_SPAWN, X_WORK, win(u, tl.from, tl.to)); walk = Math.sin(Math.PI * tl.p) * 1.2; }
    else if (tl.phase === 'teardown') { tint = win(u, tl.from, tl.from + 0.8); fade = 1 - win(u, tl.from + 0.5, tl.to); sink = win(u, tl.from + 0.5, tl.to) * 0.25; }
    else if (tl.phase === 'persist') { fade = 0; }
    crab.object3d.visible = fade > 0.01 && sc > 0.01;
    crab.object3d.position.set(x, -sink, Z);
    crab.object3d.scale.setScalar(0.42 * Math.max(0.001, sc));
    crab.setWalk(walk);
    crab.setPose(tl.phase === 'write' ? 'work' : 'idle');
    crab.setTint(C.husk, tint);
    crab.setOpacity(fade);
    crab.update(t, dt);

    // pulse path: claw (y .3) → hole (y 0) → crate top
    const wp = tl.phase === 'write' ? clamp01((u - tl.from) / 1.2) : -1;
    pulse.visible = wp >= 0 && wp < 1;
    if (pulse.visible) {
      const e = ease(wp);
      const yTop = 0.32, yEnd = target.c.position.y + S / 2 + 0.08;
      pulse.position.set(lerp(0.3, 0.05, Math.min(1, e * 3)), lerp(yTop, yEnd, e), lerp(Z + 0.15, 0.75, Math.min(1, e * 3)));
      pulse.rotation.y = e * Math.PI;
    }
    // target crate flashes on arrival then settles to its persisted glow (seamless)
    const arrive = tl.from + (tl.phase === 'write' ? 1.2 : 0);
    let flash = 0;
    if (tl.phase === 'write' && u >= arrive) flash = 1 - clamp01((u - arrive) / 0.6) * 0.3;
    else if (tl.phase === 'teardown' || tl.phase === 'persist') {
      const since = u - (PHASES[0].d + PHASES[1].d + 1.2);
      flash = 0.7 * Math.exp(-since / 1.4);
    }
    target.m.emissiveIntensity = target.base + flash * 0.9;
  }

  return { object3d: g, meta: burrowMeta, crab, update, dispose() { crab.dispose(); k.dispose(); } };
}
