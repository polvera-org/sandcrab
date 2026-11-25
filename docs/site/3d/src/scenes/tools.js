// Tools = what crabd does inside the sandbox. Centre: the toolbelt crab (kit scene)
// flipping through native tools. Left: the tide clock — a bash timeout sweeps, then
// the whole process group (parent + children) is killed together. Right: a few
// grains — output piles up, the middle is dropped, head and tail are kept.
// One idea: "native tools, with hard limits on time and output."
// Phases (18 s, matches the toolbelt loop): clock 9 (sweep 6 → kill 1.2 → hold 1.8)
//   then grains 9 (pile 4 → cut 1.4 → hold 2.6 → clear 1).
// Motion budget: toolbelt card flip (kit), one side's mechanism at a time, slow sway.
import { THREE, createKit, PALETTE as C, ease, win, clamp01, lerp, TAU } from '../kit.js';
import { createToolbelt } from '../toolbelt.js';
import { anchor, label, loopU } from './common.js';

export const toolsMeta = {
  title: 'Tools',
  fallback: 'toolbelt',
  alt: 'A crab with a toolbelt; beside it a clock that kills a whole process tree on timeout, and a pile of output grains with the middle dropped',
  loop: 18,
  still: 14.6,
  phases: [{ name: 'clock', d: 9 }, { name: 'grains', d: 9 }],
  view: { target: [0, 0.7, 0.1], radius: 2.95, yaw: 0.36, pitch: 0.44, fraction: 0.9, sway: 0.03 },
};

const LAYERS = 12, PER = 9;

export function createTools() {
  const k = createKit();
  const g = new THREE.Group();

  const belt = createToolbelt();
  belt.object3d.scale.setScalar(0.62);
  g.add(belt.object3d);

  // ── tide clock + process tree (left) ──
  const left = new THREE.Group();
  left.position.set(-2.55, 0, 0.2);
  g.add(left);
  left.add(k.disc(0.95, 0.22));
  const clock = new THREE.Group();
  clock.position.set(-0.25, 0, -0.15);
  left.add(clock);
  const face = k.mesh(new THREE.CylinderGeometry(0.42, 0.42, 0.08, 36).rotateX(Math.PI / 2), k.toon(C.paper));
  face.position.y = 0.62;
  clock.add(face);
  const rim = k.mesh(new THREE.TorusGeometry(0.43, 0.04, 6, 36), k.toon(C.teal700), { outline: false });
  rim.position.set(0, 0.62, 0.02);
  clock.add(rim);
  const stand = k.mesh(new THREE.BoxGeometry(0.1, 0.22, 0.1).translate(0, 0.11, 0), k.toon(C.teal700));
  clock.add(stand);
  // the elapsed sector (teal wedge) and the hand
  // full disc starting at 12 o'clock, mirrored so it fills clockwise; drawRange reveals it
  const sector = new THREE.Mesh(k.geo(new THREE.CircleGeometry(0.36, 48, Math.PI / 2, TAU)), k.basic(C.teal500, { transparent: true, opacity: 0.55, side: THREE.DoubleSide }));
  sector.scale.x = -1;
  sector.position.set(0, 0.62, 0.045);
  clock.add(sector);
  const hand = new THREE.Group();
  hand.position.set(0, 0.62, 0.06);
  const needle = k.mesh(new THREE.BoxGeometry(0.035, 0.34, 0.02).translate(0, 0.17, 0), k.toon(C.ink), { outline: false });
  hand.add(needle);
  clock.add(hand);
  // process group: parent + two children on stems, linked by ink lines
  const tree = new THREE.Group();
  tree.position.set(0.38, 0, 0.25);
  left.add(tree);
  const mProc = k.toon(C.teal500);
  const nodes = [[0, 0.62, 0], [-0.22, 0.28, 0.05], [0.22, 0.28, -0.05]].map(([x, y, z]) => {
    const n = k.mesh(new THREE.BoxGeometry(0.17, 0.17, 0.17), mProc);
    n.position.set(x, y, z);
    tree.add(n);
    return n;
  });
  const linkGeo = k.geo(new THREE.BufferGeometry().setFromPoints([
    new THREE.Vector3(0, 0.62, 0), new THREE.Vector3(-0.22, 0.28, 0.05),
    new THREE.Vector3(0, 0.62, 0), new THREE.Vector3(0.22, 0.28, -0.05),
  ]));
  const links = new THREE.LineSegments(linkGeo, k.mat(new THREE.LineBasicMaterial({ color: C.ink, transparent: true })));
  tree.add(links);

  // ── a few grains (right) ──
  const right = new THREE.Group();
  right.position.set(2.55, 0, 0.2);
  g.add(right);
  right.add(k.disc(0.95, 0.22));
  const tray = k.mesh(new THREE.BoxGeometry(0.62, 0.06, 0.62).translate(0, 0.03, 0), k.toon(C.teal700));
  right.add(tray);
  const G = 0.11;
  const grains = new THREE.InstancedMesh(k.geo(new THREE.BoxGeometry(G, G, G)), k.toon(C.sandDeep), LAYERS * PER);
  grains.castShadow = true;
  right.add(grains);
  const mark = k.mesh(new THREE.BoxGeometry(0.5, 0.02, 0.5), k.toon(C.ink), { outline: false });
  right.add(mark);
  const m4 = new THREE.Matrix4(), v = new THREE.Vector3(), s3 = new THREE.Vector3(), qI = new THREE.Quaternion();

  const labels = [
    label('16 native tools in crabd', anchor(g, 0, 1.75, 0)),
    label('timeout → kill the whole group', anchor(left, 0, 1.25, 0)),
    label('output: head + tail kept', anchor(right, 0, 1.6, 0)),
  ];

  function update(t, dt) {
    const u = loopU(t, toolsMeta.loop);
    belt.update(t, dt);

    // clock: sweep 0→6 s, kill 6→7.2, hold → 9, then hand returns while grains play
    const sweep = u < 6 ? u / 6 : u < 9 ? 1 : 1 - win(u, 9, 10.2);
    hand.rotation.z = -TAU * sweep;
    sector.geometry.setDrawRange(0, 3 * Math.round(48 * sweep));
    sector.material.color.set(u >= 6 && u < 9 ? C.ink : C.teal500);
    // the group dies together at the timeout, then a fresh call's tree grows back (9→10.2)
    const kill = u < 6 ? 0 : u < 7.2 ? win(u, 6, 7.2) : u < 9 ? 1 : 1 - win(u, 9, 10.2);
    nodes.forEach((n, i) => {
      const k2 = kill;
      n.scale.setScalar(Math.max(0.001, 1 - k2));
      n.position.y = [0.62, 0.28, 0.28][i] - 0.2 * k2;
    });
    links.material.opacity = 1 - kill;

    // grains: layers fall in one by one (9→13), middle layers drop out (13→14.4), clear at 17→18
    const pile = clamp01((u - 9) / 4) * LAYERS;
    const cut = win(u, 13, 14.4);
    const clear = win(u, 17, 18);
    const KEEP = 3; // head + tail layers
    for (let l = 0; l < LAYERS; l++) {
      const inMiddle = l >= KEEP && l < LAYERS - KEEP;
      const arrive = clamp01(pile - l);
      for (let j = 0; j < PER; j++) {
        const i = l * PER + j;
        const gx = ((j % 3) - 1) * (G + 0.01), gz = (Math.floor(j / 3) - 1) * (G + 0.01);
        // the tail layers close the gap only partly, leaving a visible break
        let y = 0.06 + G / 2 + l * (G + 0.008);
        if (!inMiddle && l >= LAYERS - KEEP) y -= cut * (LAYERS - 2 * KEEP) * (G + 0.008) * 0.75;
        let sc = arrive > 0 ? 1 : 0;
        if (u < 9 || u >= 17 + 0.999) sc = 0;
        y += (1 - ease(arrive)) * 0.5;
        let x = gx;
        if (inMiddle) { sc *= 1 - cut; x += cut * (0.5 + 0.1 * j); y -= cut * 0.25; }
        sc *= 1 - clear;
        v.set(x, y, gz);
        s3.setScalar(Math.max(0.0001, sc));
        m4.compose(v, qI, s3);
        grains.setMatrixAt(i, m4);
      }
    }
    grains.instanceMatrix.needsUpdate = true;
    const gapY = 0.06 + KEEP * (G + 0.008) + (G + 0.008) * 0.35;
    mark.visible = cut > 0.6 && clear < 0.5;
    mark.position.set(0, gapY, 0);
    mark.scale.setScalar(Math.max(0.001, ease(clamp01((cut - 0.6) / 0.4))));

    const onClock = u < 9;
    labels[1].a = onClock ? 1 : 0.4;
    labels[2].a = onClock ? 0.4 : 1;
    labels[0].a = 0.95;
  }

  return { object3d: g, meta: toolsMeta, labels, update, dispose() { belt.dispose(); k.dispose(); } };
}
