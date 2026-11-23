// Mounts = workspaces outlive sandboxes. The kit burrow (crabs come and go, crates
// stay) with a read-only window and a read-write door on its roof. Beside it, a
// Firecracker jar: the workspace is packed into an ext4 drive, carried into the
// jar at spawn and synced back at teardown. Between turns the host drops a file in.
// One idea: "the burrow persists; every shell, every backend, writes back into it."
// Phases (10 s, the burrow loop): spawn+walk 2.8 (drive → jar) → write 1.8 (drive in
//   jar) → teardown 1.8 (drive syncs back) → persist 3.6 (host put_file).
// Motion budget: burrow crab (kit), the drive or the host file (never both), sway.
import { THREE, createKit, PALETTE as C, ease, win, clamp01, lerp } from '../kit.js';
import { createBurrow } from '../burrow.js';
import { createJarBody } from '../jar.js';
import { anchor, label, loopU } from './common.js';

export const mountsMeta = {
  title: 'Mounts',
  fallback: 'burrow',
  alt: 'A burrow of crates that persists while crabs come and go; an ext4 drive carries it into a Firecracker jar and back',
  loop: 10,
  still: 2.1,
  phases: [{ name: 'spawn', d: 2.8 }, { name: 'write', d: 1.8 }, { name: 'teardown', d: 1.8 }, { name: 'persist', d: 3.6 }],
  groundY: -1.8 * 0.8,
  view: { target: [0.55, -0.3, 0.2], radius: 2.95, yaw: 0.34, pitch: 0.42, fraction: 0.88, sway: 0.04 },
};

const BS = 0.8; // burrow scale

export function createMounts() {
  const k = createKit();
  const g = new THREE.Group();

  const burrow = createBurrow();
  burrow.object3d.scale.setScalar(BS);
  burrow.object3d.position.x = -1.0;
  g.add(burrow.object3d);
  const hole = new THREE.Vector3(-1.0 + 0.05 * BS, 0, 0.75 * BS);

  // ro window + rw door on the roof, at the back
  const mFrame = k.toon(C.teal700);
  const win_ = new THREE.Group();
  const ring = k.mesh(new THREE.TorusGeometry(0.2, 0.05, 6, 24), mFrame);
  ring.position.y = 0.3;
  const glass = new THREE.Mesh(k.geo(new THREE.CircleGeometry(0.19, 24)), k.glass({ opacity: 0.35, rimOpacity: 0.35 }));
  glass.position.y = 0.3;
  const post = k.mesh(new THREE.BoxGeometry(0.06, 0.12, 0.06).translate(0, 0.06, 0), mFrame);
  win_.add(ring, glass, post);
  win_.position.set(-1.95, 0, -0.75);
  g.add(win_);
  const door = new THREE.Group();
  const jamb = k.mesh(new THREE.BoxGeometry(0.42, 0.62, 0.06).translate(0, 0.31, 0), mFrame);
  const leaf = k.mesh(new THREE.BoxGeometry(0.3, 0.52, 0.04).translate(0.15, 0.26, 0), k.toon(C.paper));
  leaf.position.set(-0.15, 0.03, 0.06);
  leaf.rotation.y = -0.75; // ajar
  const gap = k.mesh(new THREE.BoxGeometry(0.3, 0.52, 0.02).translate(0, 0.29, 0), k.toon(C.ink), { outline: false });
  gap.position.z = 0.025;
  door.add(jamb, gap, leaf);
  door.position.set(-0.95, 0, -0.85);
  g.add(door);

  // Firecracker jar on its own disc
  const jarBase = k.mesh(new THREE.BoxGeometry(1.6, 1.8 * BS, 1.6).translate(0, -0.9 * BS, 0), k.toon(C.sand200), { receive: true });
  jarBase.position.set(2.15, 0, 0.35);
  g.add(jarBase);
  const jar = createJarBody({ scale: 0.5, sealed: true, crab: { pose: 'work' } });
  jar.object3d.position.set(2.15, 0, 0.35);
  g.add(jar.object3d);
  const slot = new THREE.Vector3(2.15, 0.12, 0.35);

  // the ext4 drive (a block of crates packed flat)
  const drive = new THREE.Group();
  const dMat = k.toon(C.teal700, { emissive: new THREE.Color(C.teal500), emissiveIntensity: 0.25 });
  drive.add(k.mesh(new THREE.BoxGeometry(0.5, 0.14, 0.36), dMat));
  const lab = k.mesh(new THREE.BoxGeometry(0.3, 0.05, 0.37), k.toon(C.paper), { outline: false });
  lab.position.y = 0.02;
  drive.add(lab);
  g.add(drive);

  // the host's file (between turns)
  const put = k.mesh(new THREE.BoxGeometry(0.16, 0.16, 0.16), k.toon(C.teal500, { emissive: new THREE.Color('#7FD3E0'), emissiveIntensity: 0.8 }));
  g.add(put);

  const labels = [
    label('workspace: the files stay', anchor(g, -1.0, -0.55, 0.9)),
    label('ro window · rw door', anchor(g, -1.45, 0.95, -0.8)),
    label('Firecracker: ext4 drive in, synced back out', anchor(g, 2.15, 1.45, 0.35), { a: 0.5 }),
    label('host puts a file between turns', anchor(g, -0.95, 1.1, 0.6), { a: 0 }),
  ];

  const tmp = new THREE.Vector3();
  function arc(a, b, p, h) { tmp.lerpVectors(a, b, ease(p)); tmp.y += Math.sin(Math.PI * clamp01(p)) * h; return tmp; }

  function update(t, dt) {
    const u = loopU(t, 10);
    burrow.update(t, dt);
    jar.update(t, dt);
    // drive: pack at the shaft (0.4–1.0), arc to the jar (1.0–2.8), sit in it while the
    // crab writes, arc back (4.6–6.0), unpack into the shaft (6.0–6.4)
    let vis = true, sc = 1, glow = 0.25;
    if (u < 0.4 || u >= 6.4) vis = false;
    else if (u < 1.0) { drive.position.copy(hole).setY(0.12); sc = ease((u - 0.4) / 0.6); }
    else if (u < 2.8) { drive.position.copy(arc(hole.clone().setY(0.12), slot, (u - 1.0) / 1.8, 1.1)); }
    else if (u < 4.6) { drive.position.copy(slot); sc = 0.7; glow = 0.25 + 0.5 * Math.sin(Math.PI * (u - 2.8) / 1.8); }
    else if (u < 6.0) { drive.position.copy(arc(slot, hole.clone().setY(0.12), (u - 4.6) / 1.4, 1.1)); }
    else { drive.position.copy(hole).setY(0.12 - 0.3 * (u - 6.0) / 0.4); sc = 1 - ease((u - 6.0) / 0.4); }
    drive.visible = vis && sc > 0.01;
    drive.scale.setScalar(Math.max(0.001, sc));
    dMat.emissiveIntensity = glow;
    // host put_file: a cube drops into the shaft while no sandbox runs (7.4–8.6)
    const pp = (u - 7.4) / 1.2;
    put.visible = pp >= 0 && pp < 1;
    if (put.visible) put.position.set(hole.x, lerp(1.3, -0.15, ease(pp)), hole.z);

    labels[2].a = u >= 0.8 && u < 6.6 ? 1 : 0.5;
    labels[3].a = u >= 7.0 && u < 9.6 ? win(u, 7.0, 7.5) * (1 - win(u, 9.1, 9.6)) : 0;
  }

  return {
    object3d: g, meta: mountsMeta, labels, update,
    dispose() { burrow.dispose(); jar.dispose(); k.dispose(); },
  };
}
