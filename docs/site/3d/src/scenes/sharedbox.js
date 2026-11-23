// Shared box = the problem. Three agents (crabs) send real commands to one shared
// machine that holds all their files. One bad command knocks a neighbour's file off.
// One idea: "one shared box means one agent can break everyone's work."
// Phases (12 s): work 4.8 (three requests) → strike 1.2 → topple 1.4 → hold 2.8 → reset 1.8.
// Motion budget: one request cube at a time, crab work pose, the toppling crate.
import { THREE, createKit, PALETTE as C, ease, win, clamp01, lerp, loopLength, timeline } from '../kit.js';
import { createCrab } from '../crab.js';
import { anchor, label, crate, serverTower, faceToward, loopU } from './common.js';

const PHASES = [
  { name: 'work', d: 4.8 }, { name: 'strike', d: 1.2 }, { name: 'topple', d: 1.4 },
  { name: 'hold', d: 2.8 }, { name: 'reset', d: 1.8 },
];

export const sharedboxMeta = {
  title: 'One shared box',
  fallback: 'reef',
  alt: 'Three crabs send commands to one shared machine; one command knocks a neighbour’s file off',
  loop: loopLength(PHASES),
  still: 8.6,
  phases: PHASES,
  view: { target: [0, 0.75, 0], radius: 2.3, yaw: 0.45, pitch: 0.5, fraction: 0.8, sway: 0.05 },
};

export function createSharedbox() {
  const k = createKit();
  const g = new THREE.Group();
  g.add(k.slab(5.2, 3.6, 0.3));

  const box = serverTower(k, 1.5, 0.95, 1.15);
  g.add(box.group);
  const TOP = 0.95;
  const owners = [];
  const spots = [[-0.42, -0.22], [0.0, 0.25], [0.45, -0.2]];
  spots.forEach(([x, z]) => {
    const c = crate(k, 0.34, 0.12);
    c.mesh.position.set(x, TOP, z);
    g.add(c.mesh);
    owners.push(c);
  });

  const crabs = [[-2.0, 0.6], [2.0, 0.7], [-0.2, 1.55]].map(([x, z], i) => {
    const c = createCrab({ scale: 0.4, pose: 'work', phase: i * 0.6, detail: 'low' });
    c.object3d.position.set(x, 0, z);
    faceToward(c.object3d, 0, 0);
    g.add(c.object3d);
    return c;
  });

  // request cube (teal) and the bad command (ink)
  const req = k.mesh(new THREE.BoxGeometry(0.16, 0.16, 0.16), k.toon(C.teal500, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.4 }));
  g.add(req);
  const bad = k.mesh(new THREE.BoxGeometry(0.2, 0.2, 0.2), k.toon(C.ink));
  g.add(bad);

  const aBox = anchor(g, 0.3, 1.5, 0);
  const aHit = anchor(g, -1.1, 0, 0.35);
  const labels = [label('one shared machine', aBox), label('one bad command → a neighbour’s files', aHit, { a: 0, below: true })];

  const victim = owners[0];
  const husk = new THREE.Color(C.husk), teal = new THREE.Color(C.teal500);
  const from = (i) => new THREE.Vector3(crabs[i].object3d.position.x * 0.8, 0.45, crabs[i].object3d.position.z * 0.8);
  const to = new THREE.Vector3(0, TOP + 0.45, 0);
  const tmp = new THREE.Vector3();

  function arc(obj, a, b, p, h = 0.7) {
    tmp.lerpVectors(a, b, ease(p));
    tmp.y += Math.sin(Math.PI * p) * h;
    obj.position.copy(tmp);
  }

  function update(t, dt) {
    const L = sharedboxMeta.loop;
    const u = loopU(t, L);
    const tl = timeline(PHASES, u);
    crabs.forEach((c) => c.update(t, dt));

    // three requests, one per crab, during 'work'
    req.visible = false;
    if (tl.phase === 'work') {
      const slot = Math.floor(u / 1.6), p = (u - slot * 1.6) / 1.2;
      if (p < 1) { req.visible = true; arc(req, from(slot), to, p); }
    }
    // the bad command, from the front crab
    bad.visible = tl.phase === 'strike';
    if (bad.visible) arc(bad, from(2), new THREE.Vector3(-0.3, TOP + 0.2, -0.1), tl.p, 0.9);

    // hit: box LED flashes, all crates jolt, the victim topples off the left edge
    const strikeEnd = PHASES[0].d + PHASES[1].d;
    const flash = u >= strikeEnd - 0.1 ? Math.exp(-(u - strikeEnd) * 2.5) : 0;
    box.ledMat.emissiveIntensity = 0.2 + 1.6 * Math.max(0, flash);
    const jolt = u >= strikeEnd && u < strikeEnd + 0.5 ? Math.sin((u - strikeEnd) * 40) * 0.03 * (1 - (u - strikeEnd) / 0.5) : 0;
    owners.forEach((o, i) => { if (i) o.mesh.position.y = TOP + Math.abs(jolt); });

    let fall = 0, fade = 0;
    if (tl.phase === 'topple') fall = win(u, tl.from, tl.to);
    else if (tl.phase === 'hold') fall = 1;
    else if (tl.phase === 'reset') { fall = 1; fade = win(u, tl.from, tl.from + 0.7); }
    const back = tl.phase === 'reset' ? win(u, tl.from + 0.7, tl.to) : 0;
    const [vx, vz] = spots[0];
    if (back > 0) {
      victim.mesh.position.set(vx, TOP + 0.6 * (1 - back), vz);
      victim.mesh.rotation.set(0, 0, 0);
      victim.mesh.scale.setScalar(Math.max(0.001, back));
    } else {
      // slide to the edge, tip over, drop to the sand
      const slide = clamp01(fall / 0.35), tip = clamp01((fall - 0.2) / 0.5), drop = clamp01((fall - 0.45) / 0.55);
      victim.mesh.position.set(lerp(vx, -0.95, ease(slide)) - 0.25 * ease(drop), lerp(TOP, 0, drop * drop), vz + 0.15 * ease(drop));
      victim.mesh.rotation.set(0, 0, ease(tip) * 1.35);
      victim.mesh.scale.setScalar(1 - fade * 0.999);
    }
    victim.mat.color.copy(teal).lerp(husk, clamp01(fall * 1.4) * 0.85);
    victim.mat.emissiveIntensity = 0.12 * (1 - clamp01(fall * 1.4));

    labels[1].a = tl.phase === 'topple' ? win(u, tl.from + 0.3, tl.to) : tl.phase === 'hold' ? 1 : tl.phase === 'reset' ? 1 - win(u, tl.from, tl.from + 0.6) : 0;
  }

  return { object3d: g, meta: sharedboxMeta, labels, update, dispose() { crabs.forEach((c) => c.dispose()); k.dispose(); } };
}
